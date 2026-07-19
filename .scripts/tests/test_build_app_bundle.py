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
        plist = app.app_info_plist("1.2.3", "42")
        self.assertEqual(plist["CFBundleIdentifier"], "com.reluxworks.gramdrive")
        self.assertEqual(plist["CFBundleExecutable"], "GramDrive")
        self.assertEqual(plist["CFBundleShortVersionString"], "1.2.3")
        self.assertEqual(plist["CFBundleVersion"], "42")
        self.assertEqual(plist["LSMinimumSystemVersion"], "14.0")
        self.assertIs(plist["LSUIElement"], True)

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
        # The label must equal the plist basename SMAppService resolves.
        self.assertEqual(plist["Label"], app.AGENT_LAUNCHD_LABEL)


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
    def test_strips_the_v_and_the_git_suffix(self):
        self.assertEqual(app.marketing_version("v0.1.0"), "0.1.0")
        self.assertEqual(app.marketing_version("v0.1.0-3-gabc123"), "0.1.0")
        self.assertEqual(app.marketing_version("0.2.5-dirty"), "0.2.5")

    def test_unparseable_yields_zeros_not_a_fabricated_number(self):
        self.assertEqual(app.marketing_version("gabc123"), "0.0.0")
        self.assertEqual(app.marketing_version(""), "0.0.0")


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

    def scripted(self, extra=None, leak_get_task_allow=False):
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
                    (bin_dir / product).write_bytes(b"\xcf\xfa\xed\xfe macho")
                return 0, str(bin_dir) + "\n"
            if joined.startswith("swift build"):
                return 0, ""
            if "-d --entitlements" in joined:
                return 0, entitlements_xml_for(argv[-1])
            if "-d --verbose=4" in joined:
                return 0, f"Executable={argv[-1]}\nCDHash=deadbeef\n"
            if joined.startswith("codesign"):
                return 0, ""
            if joined.startswith("spctl"):
                return 0, "accepted\n"
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

    def stage(self, tmp: Path) -> tuple[Path, Path]:
        """A repo with the SwiftPM package and a staged core package present."""
        repo = tmp / "repo"
        (repo / app.SUPPORT_PACKAGE).mkdir(parents=True)
        core = repo / app.DEFAULT_CORE_PACKAGE
        core.mkdir(parents=True)
        (core / "Package.swift").write_text("// core")
        (core / "gramdrive-core-manifest.json").write_text(json.dumps({"contract_version": "0.5.0"}))
        return repo, core

    def run_pipeline(self, tmp: Path, *, extra=None, leak_get_task_allow=False, **kwargs):
        repo, core = self.stage(tmp)
        out = tmp / "out"
        out.mkdir()
        run, calls = self.scripted(extra, leak_get_task_allow=leak_get_task_allow)
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

    def test_assembles_the_expected_bundle_layout(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, _, out = self.run_pipeline(Path(tmp))
            appdir = out / "GramDrive.app"
            self.assertTrue((appdir / "Contents" / "MacOS" / "GramDrive").is_file())
            self.assertTrue((appdir / "Contents" / "MacOS" / "gramdrive-agent").is_file())
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

    def test_notarize_submits_staples_and_records_the_submission(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, calls, _ = self.run_pipeline(Path(tmp), notarize=True)
            self.assertTrue(manifest["notarization"]["submitted"])
            self.assertEqual(manifest["notarization"]["id"], "sub-123")
            self.assertEqual(manifest["notarization"]["status"], "Accepted")
            self.assertTrue(any("notarytool submit" in " ".join(c) for c in calls))
            self.assertTrue(any(c[:2] == ("xcrun", "stapler") for c in calls))

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
            self.assertIn("GramDrive-0.1.0.dmg", rendered)
            self.assertTrue(any(name.startswith("GramDrive.app/") for name in manifest["checksums"]))

    def test_reproducibility_claim_is_honest(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            # A signed artifact is attributable, not byte-identical.
            self.assertFalse(manifest["reproducible"]["byte_identical"])
            self.assertTrue(manifest["reproducible"]["attributable"])


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
