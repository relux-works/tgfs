#!/usr/bin/env python3
"""Package and run a zero-window accessory-host Sparkle manual-check smoke."""

from __future__ import annotations

import argparse
import base64
import contextlib
import functools
import http.server
import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
import threading
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
HOST_PACKAGE = REPO_ROOT / ".scripts" / "smoke" / "sparkle-manual-update-host"
CORE_PACKAGE = REPO_ROOT / ".temp" / "packaging" / "GramDriveCore"
DEFAULT_WORK_DIR = REPO_ROOT / ".temp" / "sparkle-manual-update-smoke"
FIXTURE_PUBLIC_ED_KEY_BYTES = bytes(
    [
        121, 17, 79, 45, 155, 141, 51, 169, 188, 110, 91, 102, 182, 147, 215, 225,
        252, 202, 110, 231, 200, 215, 62, 171, 40, 145, 237, 128, 130, 44, 150, 89,
    ]
)
FIXTURE_PUBLIC_ED_KEY = base64.b64encode(FIXTURE_PUBLIC_ED_KEY_BYTES).decode("ascii")

# Sparkle's published test-app keypair, used only by this isolated local
# fixture. Keeping it as bytes avoids mistaking fixture material for a
# production secret; the runtime key file is created below the ignored .temp/.
FIXTURE_PRIVATE_ED_KEY_BYTES = bytes(
    [
        200, 238, 135, 84, 10, 189, 3, 193, 61, 208, 203, 30, 133, 47, 12, 22,
        19, 52, 252, 99, 110, 205, 209, 94, 215, 144, 201, 70, 27, 162, 163, 108,
        0, 164, 68, 184, 226, 93, 121, 199, 172, 17, 26, 64, 89, 68, 232, 41,
        2, 26, 245, 175, 158, 165, 42, 55, 5, 97, 8, 243, 251, 164, 93, 9,
        121, 17, 79, 45, 155, 141, 51, 169, 188, 110, 91, 102, 182, 147, 215, 225,
        252, 202, 110, 231, 200, 215, 62, 171, 40, 145, 237, 128, 130, 44, 150, 89,
    ]
)
SIGNATURE_PATTERN = re.compile(r'sparkle:edSignature="([^"]+)" length="([0-9]+)"')
RESULT_PREFIX = "SPARKLE_MANUAL_UPDATE_SMOKE "


class SmokeFailure(RuntimeError):
    pass


def privacy_safe(value: str) -> str:
    return value.replace(str(REPO_ROOT), "<repo>").replace(str(Path.home()), "<home>")


def run(command: list[str], *, cwd: Path = REPO_ROOT, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode != 0:
        raise SmokeFailure(privacy_safe(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"))
    return result


def host_info_plist(feed_url: str) -> dict[str, object]:
    return {
        "CFBundleDevelopmentRegion": "en",
        "CFBundleExecutable": "SparkleManualUpdateSmokeHost",
        "CFBundleIdentifier": "com.reluxworks.gramdrive.sparkle-manual-update-smoke",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": "Sparkle Manual Update Smoke",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "1.0",
        "CFBundleVersion": "1",
        "LSMinimumSystemVersion": "14.0",
        "LSUIElement": True,
        "NSAppTransportSecurity": {"NSAllowsLocalNetworking": True},
        "SUEnableAutomaticChecks": False,
        "SUFeedURL": feed_url,
        "SUPublicEDKey": FIXTURE_PUBLIC_ED_KEY,
        "SURequireSignedFeed": True,
        "SUVerifyUpdateBeforeExtraction": True,
    }


def write_update_archive(path: Path) -> None:
    update_plist = {
        "CFBundleExecutable": "SparkleManualUpdateSmokeHost",
        "CFBundleIdentifier": "com.reluxworks.gramdrive.sparkle-manual-update-smoke",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": "2.0",
        "CFBundleVersion": "2",
    }
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr(
            "SparkleManualUpdateSmokeHost.app/Contents/Info.plist",
            plistlib.dumps(update_plist, sort_keys=True),
        )
        archive.writestr(
            "SparkleManualUpdateSmokeHost.app/Contents/MacOS/SparkleManualUpdateSmokeHost",
            b"fixture-only update bytes\n",
        )


def write_signed_feed(feed_dir: Path, sign_update: Path, private_key_file: Path, base_url: str) -> None:
    archive_path = feed_dir / "SparkleManualUpdateSmokeHost-2.zip"
    write_update_archive(archive_path)
    signature = run(
        [str(sign_update), "--ed-key-file", str(private_key_file), str(archive_path)]
    ).stdout.strip()
    match = SIGNATURE_PATTERN.fullmatch(signature)
    if match is None:
        raise SmokeFailure(f"unexpected sign_update archive output: {signature!r}")
    ed_signature, length = match.groups()
    appcast = f"""<?xml version="1.0" encoding="utf-8"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
  <channel>
    <title>Sparkle manual update smoke</title>
    <item>
      <title>Version 2.0</title>
      <sparkle:version>2</sparkle:version>
      <sparkle:shortVersionString>2.0</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
      <enclosure url="{base_url}/{archive_path.name}" length="{length}" type="application/octet-stream" sparkle:edSignature="{ed_signature}" />
    </item>
  </channel>
</rss>
"""
    appcast_path = feed_dir / "appcast.xml"
    appcast_path.write_text(appcast, encoding="utf-8")
    run([str(sign_update), "--ed-key-file", str(private_key_file), str(appcast_path)])
    run([str(sign_update), "--ed-key-file", str(private_key_file), "--verify", str(appcast_path)])


def locate_one(root: Path, name: str, *, directory: bool = False) -> Path:
    matches = [
        path
        for path in root.rglob(name)
        if (path.is_dir() if directory else path.is_file())
    ]
    if not matches:
        raise SmokeFailure(f"could not locate {name} below {root}")
    return min(
        matches,
        key=lambda path: (
            "old_dsa_scripts" in path.parts,
            "artifacts" not in path.parts,
            len(path.parts),
        ),
    )


def package_host(work_dir: Path, feed_url: str) -> tuple[Path, Path]:
    scratch = work_dir / "swift-build"
    environment = dict(os.environ)
    environment["GRAMDRIVE_CORE_PACKAGE"] = str(CORE_PACKAGE)
    run(
        [
            "swift", "build",
            "--package-path", str(HOST_PACKAGE),
            "--scratch-path", str(scratch),
            "--product", "sparkle-manual-update-smoke-host",
        ],
        env=environment,
    )
    executable = locate_one(scratch, "sparkle-manual-update-smoke-host")
    framework = locate_one(scratch, "Sparkle.framework", directory=True)
    sign_update = locate_one(scratch, "sign_update")

    app = work_dir / "SparkleManualUpdateSmokeHost.app"
    contents = app / "Contents"
    macos = contents / "MacOS"
    frameworks = contents / "Frameworks"
    macos.mkdir(parents=True)
    frameworks.mkdir(parents=True)
    packaged_executable = macos / "SparkleManualUpdateSmokeHost"
    shutil.copy2(executable, packaged_executable)
    shutil.copytree(framework, frameworks / framework.name, symlinks=True)
    with (contents / "Info.plist").open("wb") as handle:
        plistlib.dump(host_info_plist(feed_url), handle, sort_keys=True)
    (contents / "PkgInfo").write_bytes(b"APPL????")

    load_commands = run(["otool", "-l", str(packaged_executable)]).stdout
    if "@executable_path/../Frameworks" not in load_commands:
        run(
            [
                "install_name_tool", "-add_rpath", "@executable_path/../Frameworks",
                str(packaged_executable),
            ]
        )
    run(["codesign", "--force", "--sign", "-", str(packaged_executable)])
    run(["codesign", "--force", "--deep", "--sign", "-", str(app)])
    return packaged_executable, sign_update


@contextlib.contextmanager
def local_server(directory: Path):
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=str(directory))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server.server_address[1]
    finally:
        server.shutdown()
        thread.join(timeout=5)
        server.server_close()


def parse_result(stdout: str) -> dict[str, object]:
    lines = [line for line in stdout.splitlines() if line.startswith(RESULT_PREFIX)]
    if len(lines) != 1:
        raise SmokeFailure(f"expected one smoke result, found {len(lines)}")
    result = json.loads(lines[0][len(RESULT_PREFIX) :])
    expected = {
        "initialQualifyingWindowCount": 0,
        "windowIsVisible": True,
        "windowCanBecomeKey": True,
        "activationPolicy": "regular",
        "hostBuild": "1",
        "offeredBuild": "2",
    }
    for key, value in expected.items():
        if result.get(key) != value:
            raise SmokeFailure(f"unexpected {key}: {result.get(key)!r}, expected {value!r}")
    if not str(result.get("windowTitle", "")).strip():
        raise SmokeFailure("Sparkle window title is empty")
    return result


def execute(work_dir: Path) -> dict[str, object]:
    if not CORE_PACKAGE.is_dir():
        raise SmokeFailure("missing staged GramDriveCore; run `make package` first")
    temporary_root = (REPO_ROOT / ".temp").resolve()
    try:
        relative_work_dir = work_dir.relative_to(temporary_root)
    except ValueError as error:
        raise SmokeFailure("--work-dir must be below the repository .temp directory") from error
    if not relative_work_dir.parts:
        raise SmokeFailure("--work-dir cannot be the repository .temp root")
    if work_dir.exists():
        shutil.rmtree(work_dir)
    feed_dir = work_dir / "feed"
    profile_dir = work_dir / "profile"
    feed_dir.mkdir(parents=True)
    profile_dir.mkdir(parents=True)
    private_key_file = work_dir / "fixture-private-ed-key.txt"
    private_key_file.write_text(
        base64.b64encode(FIXTURE_PRIVATE_ED_KEY_BYTES).decode("ascii") + "\n",
        encoding="ascii",
    )
    private_key_file.chmod(0o600)

    with local_server(feed_dir) as port:
        base_url = f"http://127.0.0.1:{port}"
        executable, sign_update = package_host(work_dir, f"{base_url}/appcast.xml")
        write_signed_feed(feed_dir, sign_update, private_key_file, base_url)
        environment = dict(os.environ)
        environment["CFFIXED_USER_HOME"] = str(profile_dir)
        result = subprocess.run(
            [str(executable)],
            cwd=work_dir,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=20,
            check=False,
        )
    if result.returncode != 0:
        raise SmokeFailure(privacy_safe(
            f"packaged host failed ({result.returncode})\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"))
    parsed = parse_result(result.stdout)
    evidence = {
        "fixture_public_key": FIXTURE_PUBLIC_ED_KEY,
        "fixture_private_key_storage": "task-scoped temporary file only",
        "sparkle_version": "2.9.5",
        "signed_feed_verified": True,
        "result": parsed,
    }
    (work_dir / "result.json").write_text(
        json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return evidence


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK_DIR)
    arguments = parser.parse_args()
    try:
        evidence = execute(arguments.work_dir.resolve())
    except (SmokeFailure, subprocess.TimeoutExpired) as error:
        print(f"sparkle manual update smoke: FAIL: {error}", file=sys.stderr)
        return 1
    print("sparkle manual update smoke: PASS")
    print(json.dumps(evidence["result"], sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
