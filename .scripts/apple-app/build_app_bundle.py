#!/usr/bin/env python3
"""Assemble, sign, verify, and notarize the GramDrive.app macOS artifact.

One versioned source (the `apple/GramDriveSupport` SwiftPM package over the
staged Rust core) produces the shipped desktop app. This script owns what
`make package` does not: turning three SwiftPM executables into a single signed,
hardened-runtime, notarizable `GramDrive.app` and its `.dmg`, and the provenance
that makes the result attributable to a commit without embedding any credential
(NFR-052/NFR-053). Owned by TASK-260715-1dk9ik.

    python3 .scripts/apple-app/build_app_bundle.py
    python3 .scripts/apple-app/build_app_bundle.py --notarize
    python3 .scripts/apple-app/build_app_bundle.py --identity 'Developer ID Application: ...'

The bundle it assembles (macOS 14+ arm64, POL-5/DEC-017):

    GramDrive.app/Contents/
      Info.plist                     com.reluxworks.gramdrive
      PkgInfo                        APPL????
      MacOS/GramDrive                the menu-bar companion shell
      MacOS/gramdrive-agent          the launchd-run engine-hosting agent
      Library/LaunchAgents/com.reluxworks.gramdrive.agent.plist
      PlugIns/GramDriveFileProvider.appex/Contents/
        Info.plist                   com.reluxworks.gramdrive.fileprovider + NSExtension
        MacOS/GramDriveFileProvider  the NSFileProviderReplicatedExtension host

Three properties this pipeline exists to guarantee, each failing loudly:

  * **Signed and verifiable.** Every Mach-O is Developer ID signed with the
    hardened runtime and a secure timestamp, signed inside-out (nested code
    first), and the result is checked with `codesign --verify --deep --strict`
    and Gatekeeper (`spctl`). The dumped entitlements are parsed and asserted,
    not assumed.
  * **Credential-free.** No signing key, notarization key, or Telegram secret
    is read from or written to the repo. The signing identity is resolved from
    a keychain already holding it; notarization uses a keychain profile
    (`gramdrive-notary`) or ASC API-key env, never a key on disk in the tree.
    The manifest records the identity's *name and team*, never key material.
  * **Attributable.** A `manifest.json` records the commit, toolchain, core
    artifact version, per-binary bundle id + entitlements + cdhash, and (when
    notarized) the submission id and status. `CHECKSUMS.sha256` covers the
    shipped files. The signed bytes are deliberately not byte-reproducible: a
    trusted timestamp varies per signature by design, which is the whole point
    of a trusted timestamp. Attributability, not byte-identity, is what a signed
    artifact can honestly claim.

Requires: macOS with Xcode (`swift`, `codesign`, `spctl`, `hdiutil`,
`xcrun notarytool`/`stapler`), the staged core package (`make package`), and a
keychain holding the Developer ID Application identity. POL-5 makes the Apple
host the only v1 target; on any other platform the script exits 2.

Exit codes: 0 packaged, 1 a step failed, 2 the run could not start.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import plistlib
import shutil
import subprocess
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_CANNOT_START = 2

OUT_ROOT = Path(".temp") / "app-packaging"

# The SwiftPM package that builds the app's executables, and the staged core it
# links (make package). Relative to the repo root.
SUPPORT_PACKAGE = Path("apple") / "GramDriveSupport"
DEFAULT_CORE_PACKAGE = Path(".temp") / "packaging" / "GramDriveCore"

# Identity and naming, all sourced from .spec/platform-requirements.md and
# TASK-260716-1jswke, never invented here.
TEAM_ID = "262RZ595FP"
# The App Group entitlement form v1 ships: team-ID-prefixed, so a Developer-ID
# build needs no portal registration and no provisioning profile
# (platform-requirements.md line 23). group.com.reluxworks.gramdrive is the
# iOS / macOS 15+ form and is deliberately NOT used here.
APP_GROUP = f"{TEAM_ID}.com.reluxworks.gramdrive"

APP_BUNDLE_ID = "com.reluxworks.gramdrive"
FILEPROVIDER_BUNDLE_ID = "com.reluxworks.gramdrive.fileprovider"
# The agent row the packaging story owns (LOGBOOK note "packaging story should
# add the agent row to the identifier table"); derived from the namespace.
AGENT_BUNDLE_ID = "com.reluxworks.gramdrive.agent"
# The launchd label / plist basename SMAppService resolves against the app
# bundle (LaunchAtLogin.swift SMAppServiceAgentLoginItem.defaultPlistName).
AGENT_LAUNCHD_LABEL = "com.reluxworks.gramdrive.agent"

# The Swift-mangled Objective-C runtime name of the extension's principal class,
# as it is emitted into the appex binary (verified with `nm`); the class is
# defined in the GramDriveFileProvider module and is not @objc-renamed, so its
# runtime name is <module>.<class>. NSExtensionMain resolves it by this string.
FILEPROVIDER_PRINCIPAL_CLASS = "GramDriveFileProvider.GramDriveFileProviderExtension"

# The product name on every user-visible surface (POL-7); never "tgfs".
PRODUCT_NAME = "GramDrive"
APP_BUNDLE_NAME = f"{PRODUCT_NAME}.app"
APPEX_BUNDLE_NAME = "GramDriveFileProvider.appex"

# The shipped architecture (POL-5/DEC-017: macOS arm64 is the whole v1 matrix).
# Passed to every `swift build` explicitly so an x86_64 CI host cross-builds
# the same arm64 executables an arm64 host builds natively, instead of silently
# staging its own arch (TASK-260719-1dwaj8); enforced per built product with
# `lipo -archs` in build_products.
BUILD_ARCH = "arm64"

# macOS deployment floor (POL-5/DEC-017), matching Package.swift's .macOS(.v14).
MINIMUM_SYSTEM_VERSION = "14.0"

# The keychain profile the notarization step uses by default. Created out of
# band (TASK-260716-1jswke) and validated against Apple; it holds the ASC API
# key, so the key never lands in the repo or the environment here.
DEFAULT_NOTARY_PROFILE = "gramdrive-notary"

# The Developer ID Application identity the artifact is signed with. Resolved
# from a keychain that already holds it; overridable for a different signer.
DEFAULT_IDENTITY = f"Developer ID Application: Relux Works, LLC ({TEAM_ID})"
IDENTITY_ENV = "GRAMDRIVE_SIGN_IDENTITY"


@dataclass(frozen=True)
class BinarySpec:
    """One signed Mach-O in the bundle: what it is built as, where it lands,
    and how it is signed.

    `product` is the SwiftPM executable product; `install_path` is relative to
    the `.app` root; `bundle_id` and `entitlements` are what its signature
    carries. The order these appear in `BINARIES` is the *signing* order —
    nested code first — because codesign refuses to seal a bundle whose nested
    code is unsigned or was signed after it.
    """

    key: str
    product: str
    install_path: str
    bundle_id: str
    #: True for the outer `.app` (signed last, seals everything below it).
    is_app_bundle: bool = False
    #: True for the `.appex` (a nested bundle, signed as a bundle not a file).
    is_appex_bundle: bool = False


# Signing order is inside-out: appex, then agent, then the app that seals them.
BINARIES: tuple[BinarySpec, ...] = (
    BinarySpec(
        key="fileprovider",
        product="gramdrive-fileprovider",
        install_path=f"Contents/PlugIns/{APPEX_BUNDLE_NAME}",
        bundle_id=FILEPROVIDER_BUNDLE_ID,
        is_appex_bundle=True,
    ),
    BinarySpec(
        key="agent",
        product="gramdrive-agent",
        install_path="Contents/MacOS/gramdrive-agent",
        bundle_id=AGENT_BUNDLE_ID,
    ),
    BinarySpec(
        key="app",
        product="gramdrive-companion",
        install_path=".",
        bundle_id=APP_BUNDLE_ID,
        is_app_bundle=True,
    ),
)

# The main executable's name inside the app bundle (CFBundleExecutable) and the
# appex's — both read GramDrive*, never the SwiftPM product name.
APP_EXECUTABLE_NAME = "GramDrive"
APPEX_EXECUTABLE_NAME = "GramDriveFileProvider"

Runner = Callable[[Sequence[str], Path, "dict[str, str] | None"], "tuple[int, str]"]


def default_runner(
    argv: Sequence[str], cwd: Path, env: dict[str, str] | None = None
) -> tuple[int, str]:
    """Run argv in cwd, returning (exit code, combined output)."""
    try:
        proc = subprocess.run(
            list(argv),
            cwd=cwd,
            capture_output=True,
            text=True,
            env=env,
        )
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found on PATH\n"
    return proc.returncode, proc.stdout + proc.stderr


class StepFailed(Exception):
    """A packaging step failed; carries what to print and nothing else."""


# -- entitlements and Info.plists (generated, not committed) -----------------
#
# Kept in this file rather than as loose plist assets for the same reason
# build_core_artifacts.py keeps its Package.swift here: one source of truth the
# self-tests assert against, and the generated files are written into the output
# and checksummed, so they are as reviewable as a committed file would be.


def app_entitlements() -> dict:
    """The containing app's entitlements.

    App-groups only: the app shares the team-prefixed container with the agent
    and the extension. Hardened runtime is a codesign flag, not an entitlement,
    and there is deliberately no `get-task-allow` (SwiftPM's debug default sets
    it true, which fails notarization) and no App Sandbox — a Developer ID
    menu-bar shell that registers a launchd agent and File Provider domains runs
    unsandboxed, and the team-prefixed group needs no provisioning profile.
    """
    return {"com.apple.security.application-groups": [APP_GROUP]}


def agent_entitlements() -> dict:
    """The background agent's entitlements: the same shared container.

    Unsandboxed hardened runtime, matching the app. When the agent later links
    libtdjson (which links brew OpenSSL/zlib), a
    `com.apple.security.cs.disable-library-validation` exception may be needed;
    the current agent links only the Rust core staticlib, so v1 stays minimal.
    That addition belongs to the TDLib-integration/release work, recorded here
    so it is not silently forgotten.
    """
    return {"com.apple.security.application-groups": [APP_GROUP]}


def fileprovider_entitlements() -> dict:
    """The File Provider extension's entitlements.

    Sandboxed (macOS File Provider extensions run in the App Sandbox) plus the
    shared container: the extension reaches durable state and the agent's
    hydration socket only through the App Group container, which is exactly what
    the sandbox grants it. No network entitlement — DEC-006 keeps TDLib and all
    network out of the extension.
    """
    return {
        "com.apple.security.app-sandbox": True,
        "com.apple.security.application-groups": [APP_GROUP],
    }


ENTITLEMENTS: dict[str, Callable[[], dict]] = {
    "app": app_entitlements,
    "agent": agent_entitlements,
    "fileprovider": fileprovider_entitlements,
}


def app_info_plist(short_version: str, build_version: str) -> dict:
    """The containing app's Info.plist.

    LSUIElement: the shell's primary surface is a menu-bar extra, so it runs
    without a Dock icon. LSMinimumSystemVersion pins the v1 floor.
    """
    return {
        "CFBundleIdentifier": APP_BUNDLE_ID,
        "CFBundleName": PRODUCT_NAME,
        "CFBundleDisplayName": PRODUCT_NAME,
        "CFBundleExecutable": APP_EXECUTABLE_NAME,
        "CFBundlePackageType": "APPL",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleShortVersionString": short_version,
        "CFBundleVersion": build_version,
        "LSMinimumSystemVersion": MINIMUM_SYSTEM_VERSION,
        "LSUIElement": True,
        "NSHighResolutionCapable": True,
        "NSHumanReadableCopyright": "Relux Works, LLC",
    }


def appex_info_plist(short_version: str, build_version: str) -> dict:
    """The File Provider extension's Info.plist.

    The NSExtension dictionary is what makes this a File Provider extension: the
    non-UI file-provider point, the principal class the system instantiates by
    name, the document group it shares state through, and the enumeration
    capability a replicated extension advertises.
    """
    return {
        "CFBundleIdentifier": FILEPROVIDER_BUNDLE_ID,
        "CFBundleName": APPEX_EXECUTABLE_NAME,
        "CFBundleDisplayName": PRODUCT_NAME,
        "CFBundleExecutable": APPEX_EXECUTABLE_NAME,
        "CFBundlePackageType": "XPC!",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleShortVersionString": short_version,
        "CFBundleVersion": build_version,
        "LSMinimumSystemVersion": MINIMUM_SYSTEM_VERSION,
        "NSExtension": {
            "NSExtensionPointIdentifier": "com.apple.fileprovider-nonui",
            "NSExtensionPrincipalClass": FILEPROVIDER_PRINCIPAL_CLASS,
            "NSExtensionFileProviderDocumentGroup": APP_GROUP,
            "NSExtensionFileProviderSupportsEnumeration": True,
        },
    }


def agent_launchd_plist() -> dict:
    """The agent's launchd property list, embedded in the app bundle.

    SMAppService.agent(plistName:) resolves this against the app's bundle and
    registers it as a login item. BundleProgram is the agent binary's path
    relative to the bundle; AssociatedBundleIdentifiers ties the login item to
    the app in System Settings; KeepAlive lets launchd restart the coordinator
    after a crash (the "instant successor after SIGKILL" the lifecycle relies
    on).
    """
    return {
        "Label": AGENT_LAUNCHD_LABEL,
        "BundleProgram": "Contents/MacOS/gramdrive-agent",
        "RunAtLoad": True,
        "KeepAlive": True,
        "ProcessType": "Adaptive",
        "AssociatedBundleIdentifiers": [APP_BUNDLE_ID],
    }


# -- versions ----------------------------------------------------------------


def marketing_version(describe: str) -> str:
    """The CFBundleShortVersionString derived from `git describe`.

    A tag like `v0.1.0` or `v0.1.0-3-gabc` yields `0.1.0`; anything unparseable
    (no tags yet) yields `0.0.0` rather than a fabricated number. This is the
    human-facing version, distinct from the build number below.
    """
    text = describe.strip()
    if text.startswith("v"):
        text = text[1:]
    # Drop the `-<commits>-g<sha>` and `-dirty` suffixes git appends.
    head = text.split("-", 1)[0]
    parts = head.split(".")
    if head and all(part.isdigit() for part in parts) and parts:
        return head
    return "0.0.0"


# -- checksums (same shape as build_core_artifacts.py) -----------------------


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def checksum_tree(root: Path) -> dict[str, str]:
    """sha256 of every file under root, keyed by POSIX path relative to it.

    Sorted so the output is stable across filesystems.
    """
    return {
        str(path.relative_to(root).as_posix()): sha256_file(path)
        for path in sorted(root.rglob("*"))
        if path.is_file() and not path.is_symlink()
    }


def format_checksums(checksums: dict[str, str]) -> str:
    """Render checksums in `shasum -a 256 -c` format."""
    return "".join(f"{digest}  {name}\n" for name, digest in sorted(checksums.items()))


def tree_size(root: Path) -> int:
    return sum(p.stat().st_size for p in root.rglob("*") if p.is_file() and not p.is_symlink())


# -- codesign / verification argv (pure, tested) -----------------------------


def codesign_argv(
    target: Path,
    *,
    identity: str,
    entitlements: Path | None,
    timestamp: bool,
    identifier: str | None = None,
) -> tuple[str, ...]:
    """The codesign command for one target.

    `--force` re-signs (SwiftPM leaves a debug ad-hoc signature); `--options
    runtime` is the hardened runtime notarization requires; `--timestamp` embeds
    a trusted timestamp (network to Apple's TSA). Entitlements are passed per
    target so the app, agent, and extension each carry only their own.
    `--generate-entitlement-der` keeps the modern DER entitlement form current
    codesign already emits, stated explicitly so a reader sees it is intended.
    `--identifier` pins the code-signing identifier: a bundle takes it from its
    Info.plist, but a loose helper Mach-O (the agent) would otherwise default to
    its file name, so packaging sets it to the agent's bundle id.
    """
    argv: list[str] = ["codesign", "--force", "--sign", identity, "--options", "runtime"]
    if timestamp:
        argv.append("--timestamp")
    else:
        # For a dry/offline signing pass; a release artifact must timestamp.
        argv.append("--timestamp=none")
    argv.append("--generate-entitlement-der")
    if identifier is not None:
        argv += ["--identifier", identifier]
    if entitlements is not None:
        argv += ["--entitlements", str(entitlements)]
    argv.append(str(target))
    return tuple(argv)


def verify_argv(app: Path) -> tuple[str, ...]:
    """Strict, deep signature verification of the assembled bundle."""
    return ("codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app))


def entitlements_dump_argv(target: Path) -> tuple[str, ...]:
    """Dump a signed target's entitlements as a plist on stdout."""
    return ("codesign", "-d", "--entitlements", ":-", "--xml", str(target))


def spctl_exec_argv(app: Path) -> tuple[str, ...]:
    """Gatekeeper execution assessment. Accepts only a notarized+stapled app;
    an un-notarized Developer ID app is reported rejected, which is expected
    until the notarize step runs."""
    return ("spctl", "--assess", "--type", "exec", "--verbose=2", str(app))


def cdhash_probe_argv(target: Path) -> tuple[str, ...]:
    return ("codesign", "-d", "--verbose=4", str(target))


def notarize_submit_argv(
    target: Path, profile: str, keychain: Path | None = None
) -> tuple[str, ...]:
    """Submit a notarizable container (a `.dmg` or a zipped `.app`) and wait,
    using a keychain profile so no key is read here. `--output-format json`
    makes the submission id and status machine-readable for the manifest.

    `keychain` names which keychain holds the profile. Omitted, notarytool reads
    the login keychain (local dev, where `gramdrive-notary` already lives). CI
    stores the profile in a throwaway keychain alongside the signing identity and
    passes it here, so nothing touches the login keychain and cleanup is one
    `security delete-keychain`.
    """
    argv = [
        "xcrun",
        "notarytool",
        "submit",
        str(target),
        "--keychain-profile",
        profile,
    ]
    if keychain is not None:
        argv += ["--keychain", str(keychain)]
    argv += ["--wait", "--output-format", "json"]
    return tuple(argv)


def ditto_zip_argv(app: Path, zip_path: Path) -> tuple[str, ...]:
    """Zip the `.app` into a notarizable container. notarytool takes a
    dmg/pkg/zip, never a bare `.app`; `--keepParent` keeps the `.app` directory
    as the archive root so the submission's code is the app itself."""
    return ("ditto", "-c", "-k", "--keepParent", str(app), str(zip_path))


def staple_argv(target: Path) -> tuple[str, ...]:
    """Staple the notarization ticket into a `.app` or a `.dmg`. The ticket is
    looked up by the target's cdhash, so the code must have been notarized (its
    cdhash registered with Apple) before this can succeed."""
    return ("xcrun", "stapler", "staple", str(target))


def hdiutil_argv(app_staging: Path, dmg: Path, volname: str) -> tuple[str, ...]:
    """Build a compressed dmg from a folder holding the .app."""
    return (
        "hdiutil",
        "create",
        "-volname",
        volname,
        "-srcfolder",
        str(app_staging),
        "-ov",
        "-format",
        "UDZO",
        str(dmg),
    )


# -- parsing (pure, tested) --------------------------------------------------


def parse_entitlements(output: str) -> dict:
    """Parse codesign's dumped entitlements plist out of its stdout.

    codesign may print a header before the XML, so the plist is located rather
    than assumed to be the whole output: from the first `<?xml`/`<plist` to the
    closing `</plist>`. A target with no entitlements yields an empty dict.
    """
    start = output.find("<?xml")
    if start == -1:
        start = output.find("<plist")
    end = output.rfind("</plist>")
    if start == -1 or end == -1:
        return {}
    fragment = output[start : end + len("</plist>")]
    try:
        parsed = plistlib.loads(fragment.encode("utf-8"))
    except Exception:  # noqa: BLE001 - a malformed dump is "no entitlements found"
        return {}
    return parsed if isinstance(parsed, dict) else {}


def parse_cdhash(output: str) -> str | None:
    """Pull the CDHash out of `codesign -dvvv` output."""
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("CDHash="):
            return line.removeprefix("CDHash=").strip()
    return None


def parse_notary_submission(output: str) -> dict:
    """Pull id/status out of `notarytool submit --output-format json`.

    Located rather than assumed to be the whole of stdout: the last line that
    parses as a JSON object with an `id`. Returns {} if none is found, which the
    caller treats as a failed submission.
    """
    for line in reversed(output.strip().splitlines()):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(record, dict) and "id" in record:
            return record
    return {}


# -- the packager ------------------------------------------------------------


@dataclass
class SignedBinary:
    key: str
    bundle_id: str
    entitlements: dict
    cdhash: str | None = None


class AppPackager:
    """Runs the pipeline. Every subprocess goes through `self.runner`."""

    def __init__(
        self,
        repo_root: Path,
        out_dir: Path,
        *,
        identity: str,
        core_package: Path,
        runner: Runner = default_runner,
        echo: Callable[[str], None] = print,
        environ: dict[str, str] | None = None,
    ):
        self.repo_root = repo_root
        self.out_dir = out_dir
        self.identity = identity
        self.core_package = core_package
        self.runner = runner
        self.echo = echo
        self.environ = environ
        self.log_dir = out_dir / "logs"

    def run(self, name: str, argv: Sequence[str], *, cwd: Path | None = None, env=None) -> str:
        self.echo(f"--- {name}: {' '.join(str(a) for a in argv)}")
        code, output = self.runner(argv, cwd or self.repo_root, env)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        (self.log_dir / f"{name}.log").write_text(output, encoding="utf-8")
        if code != 0:
            raise StepFailed(
                f"{name} failed (exit {code}); log: {self.log_dir / f'{name}.log'}\n"
                f"{output[-4000:]}"
            )
        return output

    # -- inputs ----------------------------------------------------------

    def build_env(self) -> dict[str, str]:
        """The environment swift build runs under: the core package by path.

        A tdjson-linked core (BUG-260720-3i74u1) declares `-ltdjson` in its
        Package.swift; the library search path reaches ld64 through
        LIBRARY_PATH, pointing at the staged runtime library inside the core
        artifact.
        """
        env = dict(self.environ if self.environ is not None else os.environ)
        env["GRAMDRIVE_CORE_PACKAGE"] = str(self.core_package)
        if self.core_tdjson_linked():
            env["LIBRARY_PATH"] = str(self.core_package / "lib")
        env["LC_ALL"] = "C"
        return env

    def core_tdjson_linked(self) -> bool:
        """Whether the staged core links the real tdjson (its manifest says)."""
        manifest = self.core_package / "gramdrive-core-manifest.json"
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return False
        return bool(data.get("tdjson", {}).get("linked", False))

    def git_info(self) -> dict:
        code, describe = self.runner(
            ("git", "describe", "--tags", "--always", "--dirty"), self.repo_root, None
        )
        _, commit = self.runner(("git", "rev-parse", "HEAD"), self.repo_root, None)
        status_code, status = self.runner(("git", "status", "--porcelain"), self.repo_root, None)
        return {
            "describe": describe.strip() if code == 0 else "unknown",
            "commit": commit.strip() or None,
            "worktree_clean": (status.strip() == "") if status_code == 0 else None,
        }

    def build_number(self) -> str:
        """CFBundleVersion: the commit count, monotonic for Sparkle ordering."""
        code, output = self.runner(("git", "rev-list", "--count", "HEAD"), self.repo_root, None)
        value = output.strip()
        return value if code == 0 and value.isdigit() else "0"

    def toolchain_info(self) -> dict:
        versions: dict[str, str] = {}
        for name, argv in {
            "swift": ("swift", "--version"),
            "xcodebuild": ("xcodebuild", "-version"),
            "rustc": ("rustc", "--version"),
        }.items():
            code, output = self.runner(argv, self.repo_root, None)
            lines = output.strip().splitlines()
            versions[name] = lines[0].strip() if code == 0 and lines else "unavailable"
        return versions

    def core_version(self) -> str:
        """The staged core's contract version, read from its manifest."""
        manifest = self.core_package / "gramdrive-core-manifest.json"
        try:
            data = json.loads(manifest.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return "unavailable"
        return str(data.get("contract_version", "unavailable"))

    # -- steps -----------------------------------------------------------

    def build_products(self) -> Path:
        """Build every app executable in release and return the bin directory.

        Built through the same SwiftPM package the smokes use, over the staged
        core (GRAMDRIVE_CORE_PACKAGE). `swift build --product` takes one product
        at a time, so each is built in turn; the shared build graph means only
        the first pays the core-compile cost.
        """
        package = self.repo_root / SUPPORT_PACKAGE
        if not (self.core_package / "Package.swift").is_file():
            raise StepFailed(
                f"staged core package not found at {self.core_package}; run "
                f"`make package` first or pass --core-package"
            )
        env = self.build_env()
        for spec in BINARIES:
            self.run(
                f"swift-build-{spec.product}",
                (
                    "swift",
                    "build",
                    "-c",
                    "release",
                    "--arch",
                    BUILD_ARCH,
                    "--product",
                    spec.product,
                ),
                cwd=package,
                env=env,
            )
        code, output = self.runner(
            ("swift", "build", "-c", "release", "--arch", BUILD_ARCH, "--show-bin-path"),
            package,
            env,
        )
        if code != 0:
            raise StepFailed(f"could not resolve swift bin path:\n{output}")
        bin_dir = Path(output.strip().splitlines()[-1].strip())
        for spec in BINARIES:
            if not (bin_dir / spec.product).is_file():
                raise StepFailed(
                    f"swift build reported success but {spec.product} is missing "
                    f"from {bin_dir}"
                )
        # The arch gate, not a trust-me: a cross-compiling host that quietly
        # fell back to its own arch would stage binaries the shipped platform
        # cannot run. `lipo -archs` reads the built file, so the claim is about
        # the bytes, and anything but exactly the shipped arch fails here.
        for spec in BINARIES:
            output = self.run(
                f"lipo-archs-{spec.product}",
                ("lipo", "-archs", str(bin_dir / spec.product)),
            )
            archs = output.split()
            if archs != [BUILD_ARCH]:
                raise StepFailed(
                    f"{spec.product} was built for {' '.join(archs) or 'no arch'}; "
                    f"the shipped platform is {BUILD_ARCH} only (POL-5/DEC-017)"
                )
        return bin_dir

    def assemble_bundle(self, bin_dir: Path, versions: tuple[str, str]) -> Path:
        """Lay out GramDrive.app around the three built executables."""
        short_version, build_version = versions
        app = self.out_dir / APP_BUNDLE_NAME
        if app.exists():
            shutil.rmtree(app)

        contents = app / "Contents"
        (contents / "MacOS").mkdir(parents=True)
        (contents / "Library" / "LaunchAgents").mkdir(parents=True)

        # Main executable and the loose agent helper.
        shutil.copy2(bin_dir / "gramdrive-companion", contents / "MacOS" / APP_EXECUTABLE_NAME)
        shutil.copy2(bin_dir / "gramdrive-agent", contents / "MacOS" / "gramdrive-agent")

        # The appex is a nested bundle.
        appex = contents / "PlugIns" / APPEX_BUNDLE_NAME
        (appex / "Contents" / "MacOS").mkdir(parents=True)
        shutil.copy2(
            bin_dir / "gramdrive-fileprovider",
            appex / "Contents" / "MacOS" / APPEX_EXECUTABLE_NAME,
        )

        # Info.plists, PkgInfo, and the agent's launchd plist.
        write_plist(contents / "Info.plist", app_info_plist(short_version, build_version))
        (contents / "PkgInfo").write_text("APPL????", encoding="ascii")
        write_plist(
            appex / "Contents" / "Info.plist",
            appex_info_plist(short_version, build_version),
        )
        write_plist(
            contents / "Library" / "LaunchAgents" / f"{AGENT_LAUNCHD_LABEL}.plist",
            agent_launchd_plist(),
        )
        return app

    # -- runtime libraries (tdjson) --------------------------------------

    #: Where Homebrew-built dependencies live; anything a bundled library
    #: references under these prefixes must itself be bundled, or the app
    #: only runs on machines with that Homebrew tree.
    BREW_PREFIXES = ("/opt/homebrew/", "/usr/local/opt/", "/usr/local/Cellar/")

    def dylib_dependencies(self, path: Path) -> list[str]:
        """The install names `path` links against, as recorded (otool -L)."""
        code, output = self.runner(("otool", "-L", str(path)), self.repo_root, None)
        if code != 0:
            raise StepFailed(f"otool -L failed for {path}:\n{output[-2000:]}")
        deps: list[str] = []
        for line in output.splitlines()[1:]:
            stripped = line.strip()
            if not stripped:
                continue
            deps.append(stripped.split(" (")[0].strip())
        return deps

    def embed_runtime_libraries(self, app: Path) -> list[str]:
        """Embed libtdjson (and its Homebrew dependency closure) into
        `Contents/Frameworks`, rewriting every reference to `@rpath`.

        No-op for a hermetic (non-tdjson) core. The staged library carries an
        absolute install name (its own staged path — what local consumers
        load); here every copy gets `@rpath/<name>` as its id, inter-library
        references are rewritten, and each executable gets the rpath to the
        app's Frameworks directory plus the staged-path→@rpath change. Only
        the agent ever *calls* the library (DEC-006: the extension hosts no
        Telegram client); the extension merely links the shared core.
        """
        if not self.core_tdjson_linked():
            return []
        staged = self.core_package / "lib" / "libtdjson.dylib"
        if not staged.is_file():
            raise StepFailed(
                f"the core manifest claims tdjson linkage but {staged} is missing"
            )
        frameworks = app / "Contents" / "Frameworks"
        frameworks.mkdir(parents=True, exist_ok=True)

        # Copy the dependency closure: the staged library plus everything it
        # (transitively) pulls from a Homebrew tree.
        pending: list[Path] = [staged]
        bundled: dict[str, Path] = {}
        while pending:
            source = pending.pop()
            if source.name in bundled:
                continue
            copy = frameworks / source.name
            shutil.copy2(source, copy)
            copy.chmod(0o644)
            bundled[source.name] = copy
            for dep in self.dylib_dependencies(source):
                if dep.startswith(self.BREW_PREFIXES):
                    resolved = Path(dep).resolve()
                    if not resolved.is_file():
                        raise StepFailed(f"{source.name} links {dep}, which does not exist")
                    pending.append(resolved)

        # Rewrite each copy: its own id, and its references to siblings.
        for name, copy in sorted(bundled.items()):
            argv: list[str] = ["install_name_tool", "-id", f"@rpath/{name}"]
            for dep in self.dylib_dependencies(copy):
                if dep.startswith(self.BREW_PREFIXES) or dep == str(staged):
                    argv += ["-change", dep, f"@rpath/{Path(dep).name}"]
            argv.append(str(copy))
            self.run(f"fixup-{name}", tuple(argv))

        # Point every executable at the bundle's Frameworks directory.
        executables = (
            (app / "Contents" / "MacOS" / APP_EXECUTABLE_NAME, "@executable_path/../Frameworks"),
            (app / "Contents" / "MacOS" / "gramdrive-agent", "@executable_path/../Frameworks"),
            (
                app / "Contents" / "PlugIns" / APPEX_BUNDLE_NAME / "Contents" / "MacOS"
                / APPEX_EXECUTABLE_NAME,
                "@executable_path/../../../../Frameworks",
            ),
        )
        for executable, rpath in executables:
            self.run(
                f"fixup-{executable.name}",
                (
                    "install_name_tool",
                    "-change",
                    str(staged),
                    "@rpath/libtdjson.dylib",
                    "-add_rpath",
                    rpath,
                    str(executable),
                ),
            )
        self.assert_no_absolute_runtime_refs(app, sorted(bundled))
        return sorted(bundled)

    def assert_no_absolute_runtime_refs(self, app: Path, bundled: list[str]) -> None:
        """Read the shipped Mach-Os back and fail if any still loads a runtime
        library by an absolute staged or Homebrew path.

        `install_name_tool -change OLD NEW` silently no-ops when OLD no longer
        matches a recorded load command (e.g. the core artifact was relocated
        after staging, so the executable's LC_LOAD_DYLIB names a different
        absolute path). The fixup exit code is 0 either way, so the rewrite
        succeeding is not proof the result is portable — only reading the bytes
        back is. Anything under a Homebrew prefix or the staged core tree would
        make the bundle run on this build machine and fail everywhere else.
        """
        frameworks = app / "Contents" / "Frameworks"
        machos = [
            app / "Contents" / "MacOS" / APP_EXECUTABLE_NAME,
            app / "Contents" / "MacOS" / "gramdrive-agent",
            app / "Contents" / "PlugIns" / APPEX_BUNDLE_NAME / "Contents" / "MacOS"
            / APPEX_EXECUTABLE_NAME,
        ]
        machos += [frameworks / name for name in bundled]
        staged_root = str(self.core_package)
        offenders: list[str] = []
        for macho in machos:
            for dep in self.dylib_dependencies(macho):
                if dep.startswith(self.BREW_PREFIXES) or dep.startswith(staged_root):
                    offenders.append(f"{macho.name}: {dep}")
        if offenders:
            raise StepFailed(
                "shipped Mach-Os still load runtime libraries by absolute "
                "staged/Homebrew paths (the bundle would only run on this build "
                "machine):\n  " + "\n  ".join(offenders)
            )

    def write_entitlement_files(self) -> dict[str, Path]:
        """Write the generated entitlements to disk for signing and provenance.

        They live under the output (not a temp dir) so they are checksummed and
        reviewable alongside the artifact.
        """
        ent_dir = self.out_dir / "entitlements"
        if ent_dir.exists():
            shutil.rmtree(ent_dir)
        ent_dir.mkdir(parents=True)
        paths: dict[str, Path] = {}
        for key, builder in ENTITLEMENTS.items():
            path = ent_dir / f"{key}.entitlements"
            write_plist(path, builder())
            paths[key] = path
        return paths

    def sign(
        self, app: Path, entitlement_files: dict[str, Path], *, timestamp: bool
    ) -> list[SignedBinary]:
        """Sign every Mach-O inside-out, then record what each carries.

        Order is `BINARIES` order (appex, agent, app): codesign refuses to seal
        a bundle whose nested code is unsigned, so the app is signed last.
        """
        signed: list[SignedBinary] = []
        # Embedded runtime libraries first — they are nested code of
        # everything above them. Same identity and hardened runtime; no
        # entitlements (libraries carry none), and library validation admits
        # them because the team matches.
        frameworks = app / "Contents" / "Frameworks"
        if frameworks.is_dir():
            for dylib in sorted(frameworks.glob("*.dylib")):
                self.run(
                    f"codesign-frameworks-{dylib.name}",
                    codesign_argv(
                        dylib,
                        identity=self.identity,
                        entitlements=None,
                        timestamp=timestamp,
                    ),
                )
        for spec in BINARIES:
            target = app if spec.is_app_bundle else app / spec.install_path
            entitlements = entitlement_files[spec.key]
            self.run(
                f"codesign-{spec.key}",
                codesign_argv(
                    target,
                    identity=self.identity,
                    entitlements=entitlements,
                    timestamp=timestamp,
                    identifier=spec.bundle_id,
                ),
            )
            signed.append(
                SignedBinary(
                    key=spec.key,
                    bundle_id=spec.bundle_id,
                    entitlements=ENTITLEMENTS[spec.key](),
                )
            )
        return signed

    def record_unsigned(self) -> list[SignedBinary]:
        """Record the bundle's binaries without signing them — the assembly
        gate's provenance.

        No codesign runs, so `cdhash` stays None and the manifest's identity is
        "unsigned". The bundle layout, Info.plists and entitlement plists are
        real (assemble_bundle/write_entitlement_files produced them); only the
        signature is absent. This is what lets the assembly contract be gated on
        a runner that holds no Developer ID identity.
        """
        return [
            SignedBinary(
                key=spec.key,
                bundle_id=spec.bundle_id,
                entitlements=ENTITLEMENTS[spec.key](),
            )
            for spec in BINARIES
        ]

    def verify(self, app: Path, signed: list[SignedBinary]) -> None:
        """Prove the signatures: strict/deep verify, then dump and assert the
        entitlements of each binary rather than trusting they were applied."""
        self.run("codesign-verify", verify_argv(app))
        for spec in BINARIES:
            target = app if spec.is_app_bundle else app / spec.install_path
            dumped = parse_entitlements(self.run(f"entitlements-{spec.key}", entitlements_dump_argv(target)))
            expected = ENTITLEMENTS[spec.key]()
            assert_entitlements(spec.key, expected, dumped)
            record = next(b for b in signed if b.key == spec.key)
            record.cdhash = parse_cdhash(self.run(f"cdhash-{spec.key}", cdhash_probe_argv(target)))

    def assess(self, app: Path) -> str:
        """Gatekeeper assessment, recorded not gated: an un-notarized Developer
        ID app is legitimately rejected here, so this reports the verdict and
        the notarized path is what turns it to accepted."""
        code, output = self.runner(spctl_exec_argv(app), self.repo_root, None)
        (self.log_dir / "spctl.log").write_text(output, encoding="utf-8")
        verdict = "accepted" if code == 0 else "rejected"
        self.echo(f"    spctl: {verdict} (exit {code})")
        return verdict

    def build_dmg(self, app: Path, version: str, *, timestamp: bool) -> Path:
        """Stage the .app alone into a folder and build a signed dmg from it."""
        staging = self.out_dir / "dmg-staging"
        if staging.exists():
            shutil.rmtree(staging)
        staging.mkdir(parents=True)
        shutil.copytree(app, staging / APP_BUNDLE_NAME, symlinks=True)
        dmg = self.out_dir / f"{PRODUCT_NAME}-{version}.dmg"
        if dmg.exists():
            dmg.unlink()
        self.run("hdiutil", hdiutil_argv(staging, dmg, PRODUCT_NAME))
        self.run(
            "codesign-dmg",
            codesign_argv(dmg, identity=self.identity, entitlements=None, timestamp=timestamp),
        )
        shutil.rmtree(staging)
        return dmg

    def notarize_app(self, app: Path, profile: str, keychain: Path | None = None) -> dict:
        """Notarize and staple the `.app` itself, BEFORE the dmg is built, so the
        app carries its own offline ticket.

        Why this and not just the dmg: stapling the dmg leaves the `.app` inside
        it un-stapled, so a user who drags the app out of the mounted dmg gets a
        bundle with no notarization ticket — its first launch is blocked offline
        (Gatekeeper cannot reach Apple to verify). Stapling the app requires its
        cdhash be registered with Apple first, and notarytool takes a container,
        not a bare `.app` — so the app is zipped, submitted, and on Accepted the
        ORIGINAL `.app` is stapled. Doing it here, before build_dmg, means the
        copy that lands in the dmg is the stapled one (packaging review 2115).
        """
        zip_path = self.out_dir / f"{APP_BUNDLE_NAME}.notarize.zip"
        zip_path.unlink(missing_ok=True)
        self.run("ditto-zip-app", ditto_zip_argv(app, zip_path))
        output = self.run(
            "notarize-app-submit", notarize_submit_argv(zip_path, profile, keychain)
        )
        record = parse_notary_submission(output)
        status = record.get("status")
        if status != "Accepted":
            raise StepFailed(
                f"app notarization did not succeed (status={status!r}, "
                f"id={record.get('id')!r}); see the log and `notarytool log`"
            )
        self.run("staple-app", staple_argv(app))
        zip_path.unlink(missing_ok=True)
        return {"submitted": True, "target": "app", "id": record.get("id"), "status": status}

    def notarize(self, dmg: Path, profile: str, keychain: Path | None = None) -> dict:
        """Submit the dmg, wait, verify accepted, then staple. Returns the record
        for the manifest (submission id + status), never any credential."""
        output = self.run("notarize-submit", notarize_submit_argv(dmg, profile, keychain))
        record = parse_notary_submission(output)
        status = record.get("status")
        if status != "Accepted":
            raise StepFailed(
                f"notarization did not succeed (status={status!r}, "
                f"id={record.get('id')!r}); see the log and `notarytool log`"
            )
        self.run("staple", staple_argv(dmg))
        return {"submitted": True, "target": "dmg", "profile": profile, "id": record.get("id"), "status": status}


def assert_entitlements(key: str, expected: dict, dumped: dict) -> None:
    """Fail unless the dumped entitlements match what was meant to be applied.

    Two directions, both load-bearing: every expected key is present with the
    expected value (the entitlement was actually applied), and `get-task-allow`
    is absent (SwiftPM's debug default would fail notarization if it leaked
    through).
    """
    for entitlement_key, value in expected.items():
        if dumped.get(entitlement_key) != value:
            raise StepFailed(
                f"{key}: entitlement {entitlement_key!r} is {dumped.get(entitlement_key)!r}, "
                f"expected {value!r}; the signature did not carry it"
            )
    if dumped.get("com.apple.security.get-task-allow"):
        raise StepFailed(
            f"{key}: signed with com.apple.security.get-task-allow=true, which fails "
            f"notarization; the release entitlements must override SwiftPM's debug default"
        )


def write_plist(path: Path, data: dict) -> None:
    """Write a plist in the XML format codesign and the loader expect."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as handle:
        plistlib.dump(data, handle, fmt=plistlib.FMT_XML)


def build_manifest(
    *,
    product_version: dict,
    identity: str,
    signed: list[SignedBinary],
    git: dict,
    toolchain: dict,
    core_version: str,
    notarization: dict,
    source_date: str,
    is_signed: bool = True,
) -> dict:
    """The artifact's identity record.

    No key material: the identity is recorded by name and team, and
    notarization by submission id and status. The signed bytes are not
    byte-reproducible (a trusted timestamp varies per signature by design), so
    the record claims attributability, not byte-identity.

    `is_signed` is False for the unsigned assembly gate: no codesign ran, so the
    record carries no cdhashes and says so, but still attributes the assembled
    bundle to its commit, toolchain and core contract version (NFR-052).
    """
    reproducible_note = (
        "The signed artifact is attributable to a commit, toolchain and core "
        "contract version, but not byte-reproducible: Developer ID signing embeds "
        "a trusted timestamp that varies per signature by design. NFR-052 asks for "
        "attributability, which this manifest and CHECKSUMS.sha256 provide."
    )
    if not is_signed:
        reproducible_note = (
            "Assembly-only artifact (no Developer ID): the bundle was laid out and "
            "its plists generated, but no codesign ran, so there are no cdhashes. The "
            "assembled bundle is attributed to its commit, toolchain and core contract "
            "version (NFR-052); signing/notarization is the release workflow's job."
        )
    return {
        "schema": 1,
        "name": PRODUCT_NAME,
        "product_version": product_version,
        "platform": "macos-arm64",
        "binary_arch": {
            "required": BUILD_ARCH,
            "verified_by": "lipo -archs on every built product (build_products)",
        },
        "minimum_system_version": MINIMUM_SYSTEM_VERSION,
        "app_group": APP_GROUP,
        "signed": is_signed,
        "signing_identity": identity,
        "team_id": TEAM_ID,
        "binaries": [
            {
                "role": b.key,
                "bundle_id": b.bundle_id,
                "entitlements": b.entitlements,
                "cdhash": b.cdhash,
            }
            for b in signed
        ],
        "core_contract_version": core_version,
        "git": git,
        "toolchain": toolchain,
        "notarization": notarization,
        "source_date": source_date,
        "reproducible": {
            "byte_identical": False,
            "attributable": True,
            "note": reproducible_note,
        },
    }


def source_date(git: dict, environ: dict[str, str]) -> str:
    """SOURCE_DATE_EPOCH if set, else recorded as the commit — the source's
    date, not the wall clock."""
    epoch = environ.get("SOURCE_DATE_EPOCH")
    if epoch and epoch.strip().isdigit():
        return datetime.fromtimestamp(int(epoch.strip()), tz=UTC).isoformat()
    return git.get("commit") or "unknown"


def package(
    repo_root: Path,
    *,
    out_dir: Path,
    identity: str,
    core_package: Path,
    notarize: bool = False,
    notary_profile: str = DEFAULT_NOTARY_PROFILE,
    notary_keychain: Path | None = None,
    timestamp: bool = True,
    unsigned: bool = False,
    runner: Runner = default_runner,
    echo: Callable[[str], None] = print,
    environ: dict[str, str] | None = None,
) -> dict:
    """Run the whole pipeline and return the manifest.

    `unsigned=True` is the assembly gate: build the executables, lay out the
    bundle and generate its plists, then STOP before codesign — no Developer ID
    identity, no dmg, no notarization. It proves the assembly contract on an
    ordinary CI runner; signing/notarization stay in the release workflow.
    """
    packager = AppPackager(
        repo_root,
        out_dir,
        identity=identity,
        core_package=core_package,
        runner=runner,
        echo=echo,
        environ=environ,
    )
    environ = environ if environ is not None else os.environ

    git = packager.git_info()
    short_version = marketing_version(git["describe"])
    build_version = packager.build_number()

    bin_dir = packager.build_products()
    app = packager.assemble_bundle(bin_dir, (short_version, build_version))
    embedded_libraries = packager.embed_runtime_libraries(app)
    entitlement_files = packager.write_entitlement_files()

    notarization: dict = {"submitted": False}
    if unsigned:
        # Assembly-only: the bundle and its plists exist, but nothing is signed.
        signed = packager.record_unsigned()
        spctl_verdict = "not-assessed"
        dmg: Path | None = None
    else:
        signed = packager.sign(app, entitlement_files, timestamp=timestamp)
        packager.verify(app, signed)
        spctl_verdict = packager.assess(app)
        if notarize:
            # Staple the app FIRST, before the dmg is built, so the copy inside
            # the dmg carries an offline ticket too (see notarize_app).
            app_note = packager.notarize_app(app, notary_profile, notary_keychain)
        dmg = packager.build_dmg(app, short_version, timestamp=timestamp)
        if notarize:
            dmg_note = packager.notarize(dmg, notary_profile, notary_keychain)
            notarization = {
                "submitted": True,
                "profile": notary_profile,
                # The dmg's id/status stay at the top level so existing readers
                # keep working; the per-target records name which is which.
                "id": dmg_note.get("id"),
                "status": dmg_note.get("status"),
                "app": app_note,
                "dmg": dmg_note,
            }
            # Re-assess the stapled app: now it must be accepted.
            spctl_verdict = packager.assess(app)

    manifest = build_manifest(
        product_version={"short": short_version, "build": build_version},
        identity=identity,
        signed=signed,
        git=git,
        toolchain=packager.toolchain_info(),
        core_version=packager.core_version(),
        notarization=notarization,
        source_date=source_date(git, dict(environ)),
        is_signed=not unsigned,
    )
    manifest["gatekeeper"] = {"spctl": spctl_verdict, "notarized": notarization.get("submitted", False)}
    manifest["tdjson"] = {
        "linked": packager.core_tdjson_linked(),
        "embedded_libraries": embedded_libraries,
    }

    checksums: dict[str, str] = {}
    if dmg is not None:
        checksums[f"{PRODUCT_NAME}-{short_version}.dmg"] = sha256_file(dmg)
    # The app bundle's files too, so the manifest covers the whole artifact.
    for name, digest in checksum_tree(app).items():
        checksums[f"{APP_BUNDLE_NAME}/{name}"] = digest
    (out_dir / "CHECKSUMS.sha256").write_text(format_checksums(checksums), encoding="utf-8")

    manifest["sizes"] = {"app_bytes": tree_size(app)}
    if dmg is not None:
        manifest["sizes"]["dmg_bytes"] = dmg.stat().st_size
    manifest["checksums"] = checksums
    (out_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")

    echo("")
    echo(f"app:          {app}")
    if dmg is not None:
        echo(f"dmg:          {dmg} ({manifest['sizes']['dmg_bytes']:,} bytes)")
    echo(f"identity:     {identity}")
    echo(f"spctl:        {spctl_verdict}")
    echo(f"notarized:    {notarization.get('submitted', False)}")
    echo(f"version:      {short_version} ({build_version})  commit: {git['describe']}")
    return manifest


def resolve_identity(args_identity: str | None, environ: dict[str, str]) -> str:
    """The signing identity: --identity, then the env override, then the default."""
    return args_identity or environ.get(IDENTITY_ENV) or DEFAULT_IDENTITY


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Assemble, sign, and notarize GramDrive.app (or --unsigned: assemble only).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--out-dir", type=Path, default=None, help=f"default: {OUT_ROOT}")
    parser.add_argument(
        "--core-package",
        type=Path,
        default=None,
        help=f"staged GramDriveCore package (default: {DEFAULT_CORE_PACKAGE})",
    )
    parser.add_argument(
        "--identity",
        default=None,
        help=f"Developer ID Application identity (default: {IDENTITY_ENV} or the Relux Works cert)",
    )
    parser.add_argument(
        "--notarize",
        action="store_true",
        help="submit the dmg for notarization and staple it (network; Apple)",
    )
    parser.add_argument(
        "--unsigned",
        action="store_true",
        help="assembly gate: build and lay out the bundle, then stop before codesign "
        "(no Developer ID, no dmg, no notarization) — the check ordinary CI can run",
    )
    parser.add_argument(
        "--notary-profile",
        default=DEFAULT_NOTARY_PROFILE,
        help=f"notarytool keychain profile (default: {DEFAULT_NOTARY_PROFILE})",
    )
    parser.add_argument(
        "--notary-keychain",
        type=Path,
        default=None,
        help="keychain holding the notary profile (default: the login keychain). "
        "CI passes the throwaway keychain the profile was stored in.",
    )
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(list(argv) if argv is not None else None)

    if args.unsigned and args.notarize:
        parser.error("--unsigned assembles without a signature; it cannot be combined with --notarize")

    repo_root = args.repo_root.resolve()
    if sys.platform != "darwin":
        print(
            "ERROR: the GramDrive.app artifact requires macOS (swift, codesign, "
            "spctl, hdiutil, notarytool). POL-5 makes macOS arm64 the v1 target.",
            file=sys.stderr,
        )
        return EXIT_CANNOT_START

    out_dir = (args.out_dir or (repo_root / OUT_ROOT)).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    core_package = (args.core_package or (repo_root / DEFAULT_CORE_PACKAGE)).resolve()
    # The assembly gate signs nothing, so it resolves no Developer ID identity —
    # the record simply says "unsigned".
    identity = "unsigned" if args.unsigned else resolve_identity(args.identity, dict(os.environ))

    try:
        package(
            repo_root,
            out_dir=out_dir,
            identity=identity,
            core_package=core_package,
            notarize=args.notarize,
            notary_profile=args.notary_profile,
            notary_keychain=args.notary_keychain.resolve() if args.notary_keychain else None,
            unsigned=args.unsigned,
        )
    except StepFailed as failure:
        print(f"\nAPP PACKAGING FAILED\n{failure}", file=sys.stderr)
        return EXIT_FAILED
    print("\nAPP PACKAGING PASSED")
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
