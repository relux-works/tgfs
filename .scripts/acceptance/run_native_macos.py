#!/usr/bin/env python3
"""Prepare and drive the macOS native-acceptance run (TASK-260715-3oe2nr).

The release gate (`.spec/quality-and-release.md`) requires *native manual
acceptance on the support matrix*: a human opening GramDrive in Finder on a
real signed build and confirming the ten File Provider flows behave — register,
enumerate, hydrate, cancel, pin, update, restart, repair, upgrade, remove
(SYNC-* / PLAT-MAC-*, macOS-File-Provider spike gates lines 64-69). This is
that gate's harness.

It is deliberately *human-in-the-loop*, and honestly so. The Finder flows need
three things a script cannot conjure: a signed, installed `GramDrive.app`, a
real Telegram test account, and a person watching Finder. TDLib is not yet
linked into `gramdrive-agent` either (`.scripts/apple-app/README.md`), so even
"open a dataless file and get the right bytes" has no unattended path today.
Pretending otherwise — faking a domain, a hydration, a Finder redraw — would
make the gate lie. So this harness automates only what a machine can *truthfully*
check, and prepares the rest for the operator:

  * a single scenario catalog (`build_catalog`) is the source of truth for the
    run-sheet, the evidence form, and the automated probes — one edit moves all
    three, so they never drift (the same one-entrypoint rule the gate runner
    lives by);
  * a preflight establishes the environment is the one the gate means — macOS
    14+ arm64 (POL-5/DEC-017), a located `GramDrive.app`, a valid Developer ID
    signature, `fileproviderctl` on PATH, the App Group container — recording
    each finding rather than assuming it;
  * per scenario it runs the machine-checkable probes (domain present after
    registration, signature valid) and captures evidence (`fileproviderctl
    dump`, the `com.reluxworks.gramdrive` unified log, `codesign`, `stat`),
    while every Finder observation stays an explicit operator check;
  * it writes `runsheet.md` (what the operator does, with expected outcomes),
    `evidence-template.md` (what they sign), and `summary.json` into
    `.temp/acceptance/<run-id>/`, the same provenance root the gate runner uses
    (NFR-052: a result is attributable to a commit).

The overall result is never "passed". A run this script completes is
*prepared* — probes captured, docs emitted — awaiting the human sign-off the
release gate actually accepts. It does not self-close (the task checklist forbids
it). The operator fills `evidence-template.md`, and that directory attaches to
the release task.

    python3 .scripts/acceptance/run_native_macos.py --run-id accept-2026-07-19
    python3 .scripts/acceptance/run_native_macos.py --emit-runsheet -   # stdout
    python3 .scripts/acceptance/run_native_macos.py --list
    python3 .scripts/acceptance/run_native_macos.py --run-id ci --require-ready

Exit codes:
    0   the run was prepared (docs written; probes ran where the environment
        allowed) — this is NOT a claim that any scenario passed
    2   the run could not start (bad arguments, a broken catalog)
    3   --require-ready was given and the environment is not the gate's matrix

Requires nothing to *prepare* (stdlib Python 3.11+, runs on any host, which is
what lets the self-tests and CI render the docs without a Mac). To *probe* a
live run it uses `sw_vers`, `uname`, `codesign`, `spctl`, `fileproviderctl`,
`log` and `stat` — each missing tool is recorded as unavailable, never a crash.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

PROVENANCE_ROOT = Path(".temp") / "acceptance"

# A run id names a directory under .temp/acceptance/ and appears in artifact
# names. Anchored, no separators: "../../etc" must never become a write path.
# (Identical rule to the gate runner — the two share the provenance root.)
RUN_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")

EXIT_OK = 0
EXIT_CANNOT_START = 2
EXIT_NOT_READY = 3

# The v1 support matrix the release gate means (POL-5 / DEC-017): macOS 14+,
# arm64 only. The preflight checks the host against exactly this.
MIN_MACOS_MAJOR = 14
REQUIRED_ARCH = "arm64"

# Identity the probes and the run-sheet reference, sourced from
# `.spec/platform-requirements.md` (DEC-019 / POL-7) — never invented here.
APP_BUNDLE_NAME = "GramDrive.app"
PROVIDER_BUNDLE_ID = "com.reluxworks.gramdrive.fileprovider"
# The unified-log subsystem/category the companion shell logs domain work under
# (apple/.../GramDriveCompanionMain/CompanionMain.swift). Evidence capture reads
# exactly this stream.
LOG_SUBSYSTEM = "com.reluxworks.gramdrive"
LOG_DOMAIN_CATEGORY = "file-provider-domains"

# Where a Developer ID signed, installed build is looked for, in order. The
# packaging output (`.temp/app-packaging/`) comes last so an installed copy in
# /Applications wins, but a freshly packaged bundle is still probeable without
# installing it.
DEFAULT_APP_CANDIDATES: tuple[str, ...] = (
    f"/Applications/{APP_BUNDLE_NAME}",
    str(Path(".temp") / "app-packaging" / APP_BUNDLE_NAME),
)

# Token substituted in probe argv with the located app bundle path. A probe
# that needs the bundle (codesign, spctl) is skipped as unavailable when no
# bundle was found, rather than run against a bogus path.
APP_TOKEN = "{app}"


# --- The catalog -------------------------------------------------------------
# Assertions are pure predicates over a probe's (exit_code, combined_output).
# They return (ok, detail): `ok` is the machine verdict, `detail` the one line
# that explains it in the summary and the evidence form.
Assertion = Callable[[int, str], "tuple[bool, str]"]


def assert_zero(describe: str) -> Assertion:
    """Pass iff the command exited 0."""

    def check(code: int, _output: str) -> tuple[bool, str]:
        return code == 0, (describe if code == 0 else f"{describe} — exit {code}")

    return check


def assert_contains(needle: str, describe: str) -> Assertion:
    """Pass iff the command exited 0 and its output contains `needle`."""

    def check(code: int, output: str) -> tuple[bool, str]:
        if code != 0:
            return False, f"{describe} — command exit {code}"
        found = needle in output
        return found, (f"{describe}" if found else f"{describe} — {needle!r} absent")

    return check


@dataclass(frozen=True)
class Probe:
    """One machine-runnable step of a scenario.

    With an `assertion` it yields a pass/fail verdict the machine can stand
    behind. Without one it is evidence-only: the command runs and its output is
    saved for the operator and the record, but the harness makes no claim about
    it (a Finder redraw is the operator's call, not `ls`'s).
    """

    name: str
    argv: tuple[str, ...]
    purpose: str
    assertion: Assertion | None = None

    @property
    def evidence_only(self) -> bool:
        return self.assertion is None

    @property
    def needs_app(self) -> bool:
        return APP_TOKEN in self.argv


@dataclass(frozen=True)
class ManualCheck:
    """One Finder observation only a person can make.

    `action` is what the operator does; `expected` is the outcome they confirm.
    Always `pending` until a human signs it — the harness never resolves one.
    """

    name: str
    action: str
    expected: str


@dataclass(frozen=True)
class Scenario:
    key: str
    title: str
    spec_refs: tuple[str, ...]
    gate: str
    preconditions: tuple[str, ...]
    probes: tuple[Probe, ...]
    manual_checks: tuple[ManualCheck, ...]


def _dump_probe(name: str = "fileproviderctl-dump") -> Probe:
    """Evidence capture of the current File Provider domain/item state.

    Shared by most scenarios: `fileproviderctl dump` is the one command that
    shows what the system believes about GramDrive's domains and materialized
    items, so it is the before/after evidence for nearly every flow.
    """
    return Probe(
        name=name,
        argv=("fileproviderctl", "dump"),
        purpose="capture current File Provider domain + item state",
    )


def _domain_log_probe(name: str = "domain-log") -> Probe:
    """The companion shell's domain reconcile/repair unified log.

    Narrowed to the one category the code actually emits under this subsystem
    (`GramDriveCompanionMain/CompanionMain.swift`): domain registration,
    reconcile and repair. The right stream for registration/restart/update/
    repair/remove; not for hydration, which is why hydrate/cancel use the
    subsystem-wide probe below.
    """
    return Probe(
        name=name,
        argv=(
            "log",
            "show",
            "--last",
            "1h",
            "--style",
            "compact",
            "--predicate",
            f'subsystem == "{LOG_SUBSYSTEM}" && category == "{LOG_DOMAIN_CATEGORY}"',
        ),
        purpose=f"capture the domain reconcile/repair log ({LOG_SUBSYSTEM} / {LOG_DOMAIN_CATEGORY})",
    )


def _subsystem_log_probe(name: str) -> Probe:
    """The whole GramDrive unified-log stream for this window (all categories).

    Used where the relevant work is not the domain category — hydration and
    cancellation run in the extension/agent. Filtering only by subsystem
    captures whatever GramDrive logged without asserting a category the code may
    not yet emit.
    """
    return Probe(
        name=name,
        argv=(
            "log",
            "show",
            "--last",
            "1h",
            "--style",
            "compact",
            "--predicate",
            f'subsystem == "{LOG_SUBSYSTEM}"',
        ),
        purpose=f"capture the full GramDrive unified log for this window ({LOG_SUBSYSTEM}, all categories)",
    )


def build_catalog() -> tuple[Scenario, ...]:
    """The ten release-gate scenarios, in the order an operator runs them.

    Order is not incidental: registration establishes the domain every later
    scenario needs, enumeration proves it is browsable, and remove tears it
    down last. Each scenario names the requirements it proves so a reviewer can
    trace the gate line to the spec, and carries the machine probes plus the
    Finder checks that together make its verdict.
    """
    return (
        Scenario(
            key="registration",
            title="Domain registration",
            spec_refs=("PLAT-MAC-001", "SYNC-070", "gate:macOS-spike"),
            gate="the account's File Provider domain registers and appears in Finder's sidebar",
            preconditions=(
                "GramDrive.app installed and launched at least once",
                "one Telegram test account authorized in the companion",
            ),
            probes=(
                Probe(
                    name="domain-present",
                    argv=("fileproviderctl", "dump"),
                    purpose="the GramDrive provider domain is registered with the system",
                    assertion=assert_contains(
                        PROVIDER_BUNDLE_ID,
                        "GramDrive File Provider domain is registered",
                    ),
                ),
                _domain_log_probe(),
            ),
            manual_checks=(
                ManualCheck(
                    name="sidebar-entry",
                    action="open Finder after authorizing the test account",
                    expected="a GramDrive location for the account appears under Locations in the sidebar",
                ),
            ),
        ),
        Scenario(
            key="enumeration",
            title="Enumeration (dataless placeholders)",
            spec_refs=("PLAT-MAC-004", "SYNC-003", "SYNC-040", "gate:macOS-spike"),
            gate="Finder shows the chat tree as stable dataless placeholders, no content hydrated by browsing",
            preconditions=("registration scenario passed",),
            probes=(
                _dump_probe(),
                Probe(
                    name="cloudstorage-root",
                    argv=(
                        "sh",
                        "-c",
                        "ls -la@O ~/Library/CloudStorage 2>&1 || true",
                    ),
                    purpose="list the File Provider mount root and its dataless markers",
                ),
            ),
            manual_checks=(
                ManualCheck(
                    name="tree-visible",
                    action="browse the GramDrive location down to a chat folder",
                    expected=(
                        "the Account/Main/<chat> tree renders; message/media items show as "
                        "not-yet-downloaded (cloud/download badge), and browsing downloads nothing"
                    ),
                ),
                ManualCheck(
                    name="no-eager-hydrate",
                    action="watch the download indicator while scrolling a large chat folder",
                    expected="no file starts downloading merely from being listed (SYNC-040)",
                ),
            ),
        ),
        Scenario(
            key="hydrate",
            title="Hydration of a dataless file",
            spec_refs=("PLAT-MAC-004", "SYNC-041", "SYNC-042", "gate:macOS-spike"),
            gate="opening a dataless file streams the correct bytes and promotes it atomically",
            preconditions=("enumeration scenario passed", "a known synthetic fixture file in the account"),
            probes=(
                Probe(
                    name="pre-open-dataless",
                    argv=("sh", "-c", "true"),
                    purpose=(
                        "placeholder: operator records `stat -f '%Sf' <file>` before opening "
                        "to show the dataless flag; captured in the evidence form"
                    ),
                ),
                _subsystem_log_probe(name="hydration-log"),
            ),
            manual_checks=(
                ManualCheck(
                    name="open-file",
                    action="double-click a known dataless media file (e.g. a fixture image)",
                    expected="it downloads, opens, and its bytes/size match the known fixture (SYNC-042 atomic promote)",
                ),
                ManualCheck(
                    name="materialized-after",
                    action="check the file's Finder badge after it opens",
                    expected="the file now shows as downloaded/materialized, not a placeholder",
                ),
            ),
        ),
        Scenario(
            key="cancel",
            title="Cancellation of an in-flight hydration",
            spec_refs=("PLAT-MAC-004", "SYNC-043", "SYNC-005", "gate:macOS-spike"),
            gate="cancelling a download stops promptly and leaves the item safely dataless, not corrupt",
            preconditions=("a large synthetic fixture file whose download is slow enough to cancel",),
            probes=(
                _dump_probe(name="fileproviderctl-dump-after-cancel"),
                _subsystem_log_probe(name="cancel-log"),
            ),
            manual_checks=(
                ManualCheck(
                    name="cancel-download",
                    action="start downloading a large file, then cancel it from Finder's progress UI",
                    expected="the download stops promptly; no partial file is presented as complete",
                ),
                ManualCheck(
                    name="reopen-after-cancel",
                    action="open the same file again after cancelling",
                    expected="it hydrates cleanly from scratch (resumable/disposable state, SYNC-043)",
                ),
            ),
        ),
        Scenario(
            key="pin",
            title="Offline pinning",
            spec_refs=("PLAT-MAC-004", "SYNC-051", "gate:macOS-spike"),
            gate="pinning keeps content offline; it survives eviction pressure",
            preconditions=("a materialized file from the hydrate scenario",),
            probes=(_dump_probe(name="fileproviderctl-dump-pins"),),
            manual_checks=(
                ManualCheck(
                    name="keep-downloaded",
                    action='right-click a materialized file and choose "Keep Downloaded" (pin)',
                    expected="the file is marked always-kept-offline",
                ),
                ManualCheck(
                    name="pin-survives",
                    action="trigger cache pressure / eviction (or the companion's evict action) and re-check the pinned file",
                    expected="the pinned file stays materialized while unpinned content may evict (SYNC-051)",
                ),
            ),
        ),
        Scenario(
            key="update",
            title="Title/order change keeps stable identity",
            spec_refs=("PLAT-MAC-002", "SYNC-026", "SYNC-045", "gate:macOS-spike"),
            gate="a chat title/order change updates the appearance without breaking item identity",
            preconditions=("a chat visible in Finder whose Telegram title or list position can change",),
            probes=(
                _dump_probe(name="fileproviderctl-dump-before-update"),
                _domain_log_probe(name="update-log"),
            ),
            manual_checks=(
                ManualCheck(
                    name="rename-observed",
                    action="change the chat's Telegram title (or its dialog order) and let the change sync",
                    expected="the Finder folder's display name/position updates to match",
                ),
                ManualCheck(
                    name="identity-stable",
                    action="confirm a previously materialized or pinned file inside that chat after the rename",
                    expected="the file is still materialized/pinned — its identity did not change (SYNC-026/045)",
                ),
            ),
        ),
        Scenario(
            key="restart",
            title="Provider / app process restart",
            spec_refs=("PLAT-MAC-001", "SYNC-004", "SYNC-031", "NFR-004", "gate:macOS-spike"),
            gate="after a provider/app restart Finder shows stable placeholders and materialized state persists",
            preconditions=("registration + at least one materialized/pinned file",),
            probes=(
                Probe(
                    name="agent-launchctl",
                    argv=(
                        "sh",
                        "-c",
                        "launchctl print gui/$(id -u)/com.reluxworks.gramdrive.agent 2>&1 | head -40 || true",
                    ),
                    purpose="capture the companion agent's launchd state before/after restart",
                ),
                _dump_probe(name="fileproviderctl-dump-after-restart"),
            ),
            manual_checks=(
                ManualCheck(
                    name="restart-provider",
                    action="quit and relaunch GramDrive (or reboot); reopen the GramDrive location in Finder",
                    expected="the same chat tree and stable placeholders reappear (identity survives restart, SYNC-004)",
                ),
                ManualCheck(
                    name="materialized-persists",
                    action="check a file that was materialized/pinned before the restart",
                    expected="it is still materialized/pinned; no re-download of already-local content",
                ),
            ),
        ),
        Scenario(
            key="repair",
            title="User-triggered domain repair",
            spec_refs=("PLAT-MAC-004", "SYNC-070", "SYNC-071", "NFR-034", "gate:macOS-spike"),
            gate='the companion "Repair File Provider Domains" action rebuilds provider state without data loss',
            preconditions=("registration passed; ideally a domain the system has lost or a stray to clean",),
            probes=(
                _dump_probe(name="fileproviderctl-dump-before-repair"),
                _domain_log_probe(name="repair-log"),
            ),
            manual_checks=(
                ManualCheck(
                    name="run-repair",
                    action='invoke the companion menu action "Repair File Provider Domains…"',
                    expected=(
                        "a lost account domain is re-registered and recovers its existing Finder state; "
                        "strays are cleaned with downloads preserved (SYNC-071)"
                    ),
                ),
                ManualCheck(
                    name="no-total-teardown",
                    action="observe repair when the canonical account set reads empty while a domain is still registered",
                    expected="repair refuses the total teardown and leaves domains in place (TotalTeardownPolicy.refuse)",
                ),
            ),
        ),
        Scenario(
            key="upgrade",
            title="In-place upgrade",
            spec_refs=("PLAT-004", "NFR-013", "SYNC-072", "gate:macOS-spike"),
            gate="installing a newer signed build over the old one preserves domains, pins, and materialized state",
            preconditions=(
                "an older signed GramDrive.app installed with materialized/pinned content",
                "a newer signed build to install over it",
            ),
            probes=(
                Probe(
                    name="app-version-before",
                    argv=(
                        "sh",
                        "-c",
                        f"defaults read '/Applications/{APP_BUNDLE_NAME}/Contents/Info' "
                        "CFBundleShortVersionString 2>&1 || true",
                    ),
                    purpose="record the installed app version before the upgrade",
                ),
                _dump_probe(name="fileproviderctl-dump-before-upgrade"),
            ),
            manual_checks=(
                ManualCheck(
                    name="install-over",
                    action="install the newer signed build over the existing one and relaunch",
                    expected="the app upgrades; the launch reconcile re-registers the same domains without wiping Finder state",
                ),
                ManualCheck(
                    name="state-survives-upgrade",
                    action="check domains, pins, and materialized files after the upgrade",
                    expected="all survive; any DB/schema migration is transactional/resumable (NFR-013/SYNC-072)",
                ),
            ),
        ),
        Scenario(
            key="remove",
            title="Account removal / uninstall cleanup",
            spec_refs=("PLAT-MAC-004", "PLAT-004", "SYNC-062", "gate:macOS-spike"),
            gate="removing the account (or uninstalling) tears the domain down cleanly with no orphan state",
            preconditions=("registration passed for the account being removed",),
            probes=(
                _dump_probe(name="fileproviderctl-dump-after-remove"),
                Probe(
                    name="cloudstorage-after-remove",
                    argv=("sh", "-c", "ls -la ~/Library/CloudStorage 2>&1 || true"),
                    purpose="confirm the provider mount for the removed account is gone",
                ),
                _domain_log_probe(name="remove-log"),
            ),
            manual_checks=(
                ManualCheck(
                    name="remove-account",
                    action="remove the test account from the companion (or uninstall the app)",
                    expected="the account's GramDrive location disappears from Finder; the domain is unregistered",
                ),
                ManualCheck(
                    name="no-orphans",
                    action="re-check fileproviderctl dump and ~/Library/CloudStorage after removal",
                    expected="no orphan domain or mount for the removed account remains (clean removal)",
                ),
            ),
        ),
    )


# The ten scenario keys the release gate requires, in run order. The catalog is
# asserted against exactly this set so a dropped or misnamed scenario is a
# start-time error, not a silently shorter run.
REQUIRED_SCENARIO_KEYS: tuple[str, ...] = (
    "registration",
    "enumeration",
    "hydrate",
    "cancel",
    "pin",
    "update",
    "restart",
    "repair",
    "upgrade",
    "remove",
)


def validate_catalog(catalog: Sequence[Scenario]) -> None:
    """Fail loudly if the catalog is not the gate's ten scenarios.

    A native-acceptance run that quietly covers nine flows is worse than one
    that refuses to start: the missing tenth is exactly the gate a reviewer
    would trust the run to have exercised. Raises ValueError on any drift.
    """
    keys = [scenario.key for scenario in catalog]
    if keys != list(REQUIRED_SCENARIO_KEYS):
        raise ValueError(
            f"catalog scenarios {keys} do not match the required "
            f"{list(REQUIRED_SCENARIO_KEYS)} in order"
        )
    names_seen: set[str] = set()
    for scenario in catalog:
        if not scenario.spec_refs:
            raise ValueError(f"scenario {scenario.key!r} has no spec refs")
        if not scenario.probes and not scenario.manual_checks:
            raise ValueError(f"scenario {scenario.key!r} has neither probes nor manual checks")
        if not scenario.manual_checks:
            # Every gate scenario ends in a Finder observation a person makes;
            # a scenario with only machine probes would claim more than a
            # script can honestly verify.
            raise ValueError(f"scenario {scenario.key!r} has no human Finder check")
        for probe in scenario.probes:
            qualified = f"{scenario.key}.{probe.name}"
            if qualified in names_seen:
                raise ValueError(f"duplicate probe name {qualified!r}")
            names_seen.add(qualified)


# --- Environment preflight ---------------------------------------------------
Runner = Callable[[Sequence[str]], "tuple[int, str]"]
Exists = Callable[[str], bool]


PROBE_TIMEOUT_SECONDS = 60


def default_runner(argv: Sequence[str]) -> tuple[int, str]:
    """Run argv, returning (exit_code, combined output). Never raises.

    The contract mirrors the gate runner: a missing tool is a recorded 127, not
    a traceback out of the harness. It adds a timeout so a wedged evidence
    command (a `log show` that never returns, a `fileproviderctl` waiting on a
    prompt) fails that one probe instead of hanging the whole preparation.
    """
    import subprocess

    try:
        proc = subprocess.run(
            list(argv),
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_SECONDS,
        )
    except FileNotFoundError:
        return 127, f"{argv[0]}: not found on PATH\n"
    except subprocess.TimeoutExpired:
        return 124, f"{argv[0]}: timed out after {PROBE_TIMEOUT_SECONDS}s\n"
    except OSError as error:  # pragma: no cover - defensive
        return 126, f"{argv[0]}: {error}\n"
    return proc.returncode, proc.stdout + proc.stderr


@dataclass
class Environment:
    """What the preflight learned about the host, and whether it is the gate's.

    `ready` means the host is the matrix the gate means AND a signed build was
    located and verified — i.e. the machine probes can be trusted. When it is
    False the run still prepares the docs; it just cannot run the live probes,
    and `reasons` says why.
    """

    macos_version: str | None = None
    arch: str | None = None
    app_path: str | None = None
    signature_valid: bool | None = None
    gatekeeper: str | None = None
    fileproviderctl_available: bool = False
    app_group_container: str | None = None
    reasons: list[str] = field(default_factory=list)
    probe_logs: dict[str, str] = field(default_factory=dict)

    @property
    def ready(self) -> bool:
        return not self.reasons

    def as_dict(self) -> dict:
        return {
            "ready": self.ready,
            "macos_version": self.macos_version,
            "arch": self.arch,
            "app_path": self.app_path,
            "signature_valid": self.signature_valid,
            "gatekeeper": self.gatekeeper,
            "fileproviderctl_available": self.fileproviderctl_available,
            "app_group_container": self.app_group_container,
            "reasons": list(self.reasons),
        }


def _macos_major(version: str) -> int | None:
    match = re.match(r"^(\d+)", version.strip())
    return int(match.group(1)) if match else None


def preflight(
    *,
    runner: Runner,
    app_candidates: Sequence[str],
    exists: Exists,
) -> Environment:
    """Establish whether the host is the gate's matrix and a build is present.

    Records every finding (version, arch, located bundle, signature, Gatekeeper
    verdict, tool availability) into the Environment and its `probe_logs`, so
    the summary carries the evidence for `ready`/not-ready rather than just the
    boolean. Runs no destructive command.
    """
    env = Environment()

    code, out = runner(("sw_vers", "-productVersion"))
    env.probe_logs["sw_vers"] = out
    if code == 0:
        env.macos_version = out.strip()
        major = _macos_major(env.macos_version)
        if major is None:
            env.reasons.append(f"could not parse macOS version {env.macos_version!r}")
        elif major < MIN_MACOS_MAJOR:
            env.reasons.append(
                f"macOS {env.macos_version} is below the v1 minimum {MIN_MACOS_MAJOR} (POL-5/DEC-017)"
            )
    else:
        env.reasons.append("not a macOS host (sw_vers unavailable)")

    code, out = runner(("uname", "-m"))
    env.probe_logs["uname"] = out
    if code == 0:
        env.arch = out.strip()
        if env.arch != REQUIRED_ARCH:
            env.reasons.append(
                f"arch {env.arch!r} is not the v1 target {REQUIRED_ARCH!r} (POL-5/DEC-017)"
            )
    else:
        env.reasons.append("could not determine architecture (uname unavailable)")

    located = next((path for path in app_candidates if exists(path)), None)
    env.app_path = located
    if located is None:
        env.reasons.append(
            f"no {APP_BUNDLE_NAME} found in {list(app_candidates)}; install or `make package-app` first"
        )
    else:
        code, out = runner(("codesign", "--verify", "--deep", "--strict", located))
        env.probe_logs["codesign"] = out
        env.signature_valid = code == 0
        if not env.signature_valid:
            env.reasons.append(f"{located} failed codesign --verify --deep --strict")

        code, out = runner(("spctl", "--assess", "--type", "exec", "--verbose", located))
        env.probe_logs["spctl"] = out
        # Gatekeeper is recorded, not gated: an un-notarized Developer ID build
        # is legitimately rejected here, and that is a finding for the operator,
        # not a reason to refuse the run (matches the packaging pipeline).
        env.gatekeeper = "accepted" if code == 0 else "rejected"

    code, _ = runner(("sh", "-c", "command -v fileproviderctl"))
    env.fileproviderctl_available = code == 0
    if not env.fileproviderctl_available:
        env.reasons.append("fileproviderctl not on PATH (needed for domain/item probes)")

    # App Group container is best-effort context, never a readiness gate: it is
    # only resolvable from inside a signed, entitled process, so its absence
    # here is expected and non-fatal.
    home = Path.home()
    container = home / "Library" / "Group Containers" / f"262RZ595FP.{LOG_SUBSYSTEM}"
    if exists(str(container)):
        env.app_group_container = str(container)

    return env


# --- Running the scenarios ---------------------------------------------------
@dataclass
class ProbeResult:
    probe: Probe
    status: str  # "pass" | "fail" | "captured" | "skipped"
    detail: str
    log_name: str | None


def _substitute(argv: tuple[str, ...], app_path: str | None) -> tuple[str, ...]:
    if app_path is None:
        return argv
    return tuple(arg.replace(APP_TOKEN, app_path) for arg in argv)


def run_probe(
    probe: Probe,
    *,
    scenario_key: str,
    env: Environment,
    runner: Runner,
    out_dir: Path,
    write: bool,
    cache: dict[tuple[str, ...], tuple[int, str]] | None = None,
) -> ProbeResult:
    """Run one probe, save its output, and classify the result.

    A probe that needs the app bundle when none was located is `skipped` (not
    `fail`): there is nothing to verify, and calling it a failure would blame
    the operator's un-prepared host for the harness's own precondition.
    Evidence-only probes are `captured`; asserted probes are `pass`/`fail`.

    `cache` deduplicates *execution* by the substituted command: several
    scenarios capture the same `log show` / `fileproviderctl dump`, and during
    automated preparation those are identical, so running each once and writing
    the shared output to each scenario's own log file keeps the per-scenario
    evidence without paying for a slow command six times. (The run-sheet still
    lists the exact per-step command for the operator to re-run live.)
    """
    if probe.needs_app and env.app_path is None:
        return ProbeResult(probe, "skipped", "no app bundle located", None)

    argv = _substitute(probe.argv, env.app_path)
    if cache is not None and argv in cache:
        code, output = cache[argv]
    else:
        code, output = runner(argv)
        if cache is not None:
            cache[argv] = (code, output)

    log_name = f"{scenario_key}.{probe.name}.log"
    if write:
        (out_dir / log_name).write_text(output, encoding="utf-8")

    if probe.evidence_only:
        return ProbeResult(probe, "captured", probe.purpose, log_name)

    assert probe.assertion is not None
    ok, detail = probe.assertion(code, output)
    return ProbeResult(probe, "pass" if ok else "fail", detail, log_name)


@dataclass
class ScenarioResult:
    scenario: Scenario
    probe_results: list[ProbeResult]

    def summary(self) -> dict:
        return {
            "key": self.scenario.key,
            "title": self.scenario.title,
            "spec_refs": list(self.scenario.spec_refs),
            "gate": self.scenario.gate,
            # A scenario is never "passed" here: its verdict is the human's, and
            # even every probe passing only means the machine-checkable slice
            # held. This field states that explicitly.
            "machine_verdict": self._machine_verdict(),
            "human_verdict": "pending",
            "probes": [
                {
                    "name": result.probe.name,
                    "purpose": result.probe.purpose,
                    "command": list(result.probe.argv),
                    "status": result.status,
                    "detail": result.detail,
                    "log": result.log_name,
                }
                for result in self.probe_results
            ],
            "manual_checks": [
                {
                    "name": check.name,
                    "action": check.action,
                    "expected": check.expected,
                    "verdict": "pending",
                }
                for check in self.scenario.manual_checks
            ],
        }

    def _machine_verdict(self) -> str:
        asserted = [r for r in self.probe_results if r.probe.assertion is not None]
        if not asserted:
            return "no-machine-checks"
        if any(r.status == "fail" for r in asserted):
            return "fail"
        if all(r.status == "pass" for r in asserted):
            return "pass"
        return "incomplete"  # some asserted probe was skipped


# --- Document generation -----------------------------------------------------
def render_runsheet(catalog: Sequence[Scenario], *, run_id: str | None, commit: str | None) -> str:
    """The operator's step-by-step Finder run-sheet, rendered from the catalog.

    Generated, never hand-maintained: the catalog is the single source, so a
    scenario edit updates the run-sheet, the evidence form and the probes at
    once.
    """
    lines: list[str] = []
    lines.append("# GramDrive — macOS native acceptance run-sheet")
    lines.append("")
    if run_id:
        lines.append(f"Run id: `{run_id}`")
    if commit:
        lines.append(f"Commit: `{commit}`")
    lines.append("")
    lines.append(
        "This is the release-gate manual acceptance for the macOS File Provider "
        "drive (`.spec/quality-and-release.md`, macOS spike gates). A person runs "
        "it on a **real signed, installed `GramDrive.app`** with a **Telegram test "
        "account**; the harness has already captured the machine-checkable probes "
        "and the environment preflight into this run's directory."
    )
    lines.append("")
    lines.append("## Ground rules")
    lines.append("")
    lines.append("- **Synthetic fixtures only** (NFR-005): use a dedicated Telegram test account, never real personal data.")
    lines.append("- **Read-only** (NFR-014, SYNC-060): GramDrive never writes to or deletes from Telegram. If any Finder write *succeeds*, that is a failure.")
    lines.append("- Matrix: **macOS 14+ arm64** (POL-5/DEC-017). Run on the support matrix, not a VM that misreports either.")
    lines.append("- For every scenario, attach the referenced evidence and record PASS/FAIL + notes in `evidence-template.md`.")
    lines.append("")
    lines.append("## Scenarios")
    lines.append("")
    for index, scenario in enumerate(catalog, start=1):
        lines.append(f"### {index}. {scenario.title}  (`{scenario.key}`)")
        lines.append("")
        lines.append(f"- **Proves:** {scenario.gate}")
        lines.append(f"- **Spec:** {', '.join(scenario.spec_refs)}")
        lines.append("- **Preconditions:**")
        for pre in scenario.preconditions:
            lines.append(f"    - {pre}")
        lines.append("- **Harness probes (already captured for you):**")
        for probe in scenario.probes:
            kind = "assert" if probe.assertion is not None else "evidence"
            lines.append(f"    - `{probe.name}` ({kind}) — {probe.purpose}")
            lines.append(f"        - `{' '.join(probe.argv)}`")
        lines.append("- **Operator steps (Finder):**")
        for step_no, check in enumerate(scenario.manual_checks, start=1):
            lines.append(f"    {step_no}. **{check.name}** — {check.action}")
            lines.append(f"        - Expected: {check.expected}")
        lines.append("")
    lines.append("## Sign-off")
    lines.append("")
    lines.append(
        "Record each scenario's verdict and evidence in `evidence-template.md`, "
        "then attach this run's directory to the release task. A scenario passes "
        "only when its operator checks are confirmed — the harness does not and "
        "cannot pass them for you."
    )
    lines.append("")
    return "\n".join(lines)


def render_evidence_template(
    catalog: Sequence[Scenario], *, run_id: str | None, commit: str | None
) -> str:
    """The sign-off form the operator fills, rendered from the catalog."""
    lines: list[str] = []
    lines.append("# GramDrive — macOS native acceptance evidence & sign-off")
    lines.append("")
    lines.append("| Field | Value |")
    lines.append("|---|---|")
    lines.append(f"| Run id | {run_id or '(fill in)'} |")
    lines.append(f"| Commit | {commit or '(fill in)'} |")
    lines.append("| Operator | (name) |")
    lines.append("| Date | (YYYY-MM-DD) |")
    lines.append("| Build cdhash / version | (from manifest.json or `codesign -dv`) |")
    lines.append("| Host (macOS / arch) | (e.g. macOS 14.5 / arm64) |")
    lines.append("| Telegram test account | (identifier, synthetic) |")
    lines.append("")
    lines.append(
        "Fill one block per scenario. Verdict is PASS / FAIL / BLOCKED. Reference "
        "the captured probe logs (`<scenario>.<probe>.log`) and any screenshots "
        "you add to this directory."
    )
    lines.append("")
    for index, scenario in enumerate(catalog, start=1):
        lines.append(f"## {index}. {scenario.title}  (`{scenario.key}`)")
        lines.append("")
        lines.append(f"- Proves: {scenario.gate}")
        lines.append(f"- Spec: {', '.join(scenario.spec_refs)}")
        lines.append("- **Verdict:** ______  (PASS / FAIL / BLOCKED)")
        lines.append("- Operator checks:")
        for check in scenario.manual_checks:
            lines.append(f"    - [ ] **{check.name}** — expected: {check.expected}")
        lines.append("- Evidence attached: ______ (probe logs, screenshots)")
        lines.append("- Notes: ______")
        lines.append("")
    lines.append("## Overall")
    lines.append("")
    lines.append("- **Release-gate verdict:** ______ (all scenarios PASS / list failures)")
    lines.append("- Known limitations recorded: ______")
    lines.append("- Signed: ______  Date: ______")
    lines.append("")
    return "\n".join(lines)


def render_list(catalog: Sequence[Scenario]) -> str:
    lines = ["macOS native acceptance scenarios (release-gate order):", ""]
    for index, scenario in enumerate(catalog, start=1):
        lines.append(f"  {index:>2}. {scenario.key:<13} {scenario.title}")
        lines.append(f"      proves: {scenario.gate}")
        lines.append(f"      spec:   {', '.join(scenario.spec_refs)}")
        probes = ", ".join(
            f"{p.name}{'*' if p.assertion is not None else ''}" for p in scenario.probes
        )
        checks = ", ".join(c.name for c in scenario.manual_checks)
        lines.append(f"      probes: {probes or '(none)'}   (*=asserted)")
        lines.append(f"      manual: {checks}")
    return "\n".join(lines)


# --- Orchestration -----------------------------------------------------------
def git_commit(runner: Runner) -> str | None:
    code, out = runner(("git", "rev-parse", "HEAD"))
    return out.strip() if code == 0 else None


def prepare_run(
    *,
    run_id: str,
    repo_root: Path,
    catalog: Sequence[Scenario],
    runner: Runner = default_runner,
    app_candidates: Sequence[str] = DEFAULT_APP_CANDIDATES,
    exists: Exists | None = None,
    require_ready: bool = False,
    echo: Callable[[str], None] = print,
) -> tuple[dict, int]:
    """Preflight, probe what can be probed, and write the run's artifacts.

    Returns (summary, exit_code). The exit code is OK when the run was prepared,
    NOT_READY only if --require-ready was set against a non-matrix host, and the
    result field is never "passed": this harness prepares a human sign-off, it
    does not grant one.
    """
    exists = exists or (lambda path: Path(path).exists())
    out_dir = repo_root / PROVENANCE_ROOT / run_id
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    commit = git_commit(runner)
    env = preflight(runner=runner, app_candidates=app_candidates, exists=exists)

    # Persist the preflight probe outputs as their own evidence.
    for name, output in env.probe_logs.items():
        (out_dir / f"preflight.{name}.log").write_text(output, encoding="utf-8")

    started = datetime.now(UTC)
    summary: dict = {
        "schema": 1,
        "kind": "macos-native-acceptance",
        "run_id": run_id,
        "commit": commit,
        "started_at": started.isoformat(),
        "environment": env.as_dict(),
        # Stated up front so no reader mistakes a prepared run for a passed gate.
        "result": None,
        "scenarios": [],
    }

    echo(f"==> macOS native acceptance: preparing run '{run_id}'")
    echo(f"    environment ready: {env.ready}")
    for reason in env.reasons:
        echo(f"      - not ready: {reason}")

    if require_ready and not env.ready:
        summary["result"] = "environment-not-ready"
        summary["finished_at"] = datetime.now(UTC).isoformat()
        # Still emit the docs: an operator on the wrong host should get the
        # run-sheet to read, just not a green light.
        _write_docs(out_dir, catalog, run_id=run_id, commit=commit)
        (out_dir / "summary.json").write_text(
            json.dumps(summary, indent=2) + "\n", encoding="utf-8"
        )
        echo("    --require-ready set and environment is not the gate matrix; refusing.")
        echo(f"    provenance: {PROVENANCE_ROOT / run_id}")
        return summary, EXIT_NOT_READY

    # One cache for the whole run: identical probe commands across scenarios
    # (the log/dump captures) execute once, not once per scenario.
    probe_cache: dict[tuple[str, ...], tuple[int, str]] = {}
    results: list[ScenarioResult] = []
    for scenario in catalog:
        echo(f"==> scenario: {scenario.key} — {scenario.title}")
        probe_results = [
            run_probe(
                probe,
                scenario_key=scenario.key,
                env=env,
                runner=runner,
                out_dir=out_dir,
                write=True,
                cache=probe_cache,
            )
            for probe in scenario.probes
        ]
        for result in probe_results:
            echo(f"    [{result.status:<8}] {result.probe.name}: {result.detail}")
        results.append(ScenarioResult(scenario, probe_results))

    summary["scenarios"] = [result.summary() for result in results]
    summary["result"] = "prepared"
    summary["finished_at"] = datetime.now(UTC).isoformat()

    _write_docs(out_dir, catalog, run_id=run_id, commit=commit)
    (out_dir / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )

    echo("")
    echo(f"prepared {len(results)} scenarios; {sum(len(r.probe_results) for r in results)} probes captured")
    echo("this is a PREPARED run awaiting human sign-off — no scenario is passed by the harness")
    echo(f"  run-sheet:        {PROVENANCE_ROOT / run_id / 'runsheet.md'}")
    echo(f"  evidence form:    {PROVENANCE_ROOT / run_id / 'evidence-template.md'}")
    echo(f"  summary:          {PROVENANCE_ROOT / run_id / 'summary.json'}")
    return summary, EXIT_OK


def _write_docs(
    out_dir: Path, catalog: Sequence[Scenario], *, run_id: str | None, commit: str | None
) -> None:
    (out_dir / "runsheet.md").write_text(
        render_runsheet(catalog, run_id=run_id, commit=commit), encoding="utf-8"
    )
    (out_dir / "evidence-template.md").write_text(
        render_evidence_template(catalog, run_id=run_id, commit=commit), encoding="utf-8"
    )


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Prepare and drive the macOS native-acceptance run.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  run_native_macos.py --run-id accept-2026-07-19\n"
            "  run_native_macos.py --emit-runsheet -\n"
            "  run_native_macos.py --list\n"
        ),
    )
    parser.add_argument(
        "--run-id",
        help="names the provenance directory .temp/acceptance/<run-id> for this run",
    )
    parser.add_argument(
        "--app-path",
        help=f"path to the signed {APP_BUNDLE_NAME} to probe (default: /Applications then .temp/app-packaging)",
    )
    parser.add_argument(
        "--require-ready",
        action="store_true",
        help="exit 3 unless the host is macOS 14+ arm64 with a located, valid-signed build",
    )
    parser.add_argument("--list", action="store_true", help="print the scenario catalog and exit")
    parser.add_argument(
        "--emit-runsheet",
        metavar="PATH",
        help="render the run-sheet markdown to PATH ('-' for stdout) and exit",
    )
    parser.add_argument(
        "--emit-evidence-template",
        metavar="PATH",
        help="render the evidence/sign-off template to PATH ('-' for stdout) and exit",
    )
    parser.add_argument("--repo-root", type=Path, default=Path.cwd())
    args = parser.parse_args(list(argv) if argv is not None else None)

    catalog = build_catalog()
    try:
        validate_catalog(catalog)
    except ValueError as error:
        parser.error(f"catalog is invalid: {error}")

    if args.list:
        print(render_list(catalog))
        return EXIT_OK

    if args.emit_runsheet is not None:
        text = render_runsheet(catalog, run_id=None, commit=None)
        _emit(args.emit_runsheet, text)
        return EXIT_OK

    if args.emit_evidence_template is not None:
        text = render_evidence_template(catalog, run_id=None, commit=None)
        _emit(args.emit_evidence_template, text)
        return EXIT_OK

    if not args.run_id:
        parser.error("--run-id is required (or use --list / --emit-runsheet / --emit-evidence-template)")
    if not RUN_ID_RE.match(args.run_id):
        parser.error(
            f"--run-id {args.run_id!r} must be 1-64 chars of letters, digits, "
            f"'.', '_' or '-', starting alphanumeric; it names a directory"
        )

    app_candidates = (args.app_path,) if args.app_path else DEFAULT_APP_CANDIDATES
    _, exit_code = prepare_run(
        run_id=args.run_id,
        repo_root=args.repo_root.resolve(),
        catalog=catalog,
        app_candidates=app_candidates,
        require_ready=args.require_ready,
    )
    return exit_code


def _emit(path: str, text: str) -> None:
    if path == "-":
        sys.stdout.write(text)
    else:
        Path(path).write_text(text, encoding="utf-8")


if __name__ == "__main__":
    sys.exit(main())
