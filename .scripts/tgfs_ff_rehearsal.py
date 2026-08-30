#!/usr/bin/env python3
"""One-shot owner-authorized GitHub rehearsal for the fixed receive-pack lane."""

from __future__ import annotations

import base64
import datetime as dt
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


ORG = "relux-works"
PREFIX = "tgfs-ff-lander-rehearsal-260830-"
API = "https://api.github.com"
OID = re.compile(r"^[0-9a-f]{40}$")
MAX_RESPONSE = 16 * 1024 * 1024


def load_broker_module():
    path = Path(__file__).with_name("tgfs_ff_bootstrap_broker.py")
    spec = importlib.util.spec_from_file_location("tgfs_ff_bootstrap_broker_rehearsal", path)
    if spec is None or spec.loader is None:
        raise RuntimeError("broker module unavailable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


BROKER = load_broker_module()


class RehearsalError(Exception):
    pass


class Client:
    def __init__(self, token: bytearray, repository: str) -> None:
        self.token = token
        self.repository = repository
        self.git_url = f"https://github.com/{ORG}/{repository}.git"

    def close(self) -> None:
        self.token[:] = b"\0" * len(self.token)

    def request(
        self,
        url: str,
        *,
        method: str = "GET",
        body: object | bytes | None = None,
        git: bool = False,
        content_type: str | None = None,
    ) -> tuple[bytes, dict[str, str]]:
        if not (url.startswith(f"{API}/") or url.startswith(self.git_url)):
            raise RehearsalError("foreign endpoint")
        if isinstance(body, bytes) or body is None:
            encoded = body
        else:
            encoded = json.dumps(body, separators=(",", ":")).encode()
        if git:
            raw = b"x-access-token:" + bytes(self.token)
            authorization = "Basic " + base64.b64encode(raw).decode("ascii")
        else:
            authorization = "Bearer " + bytes(self.token).decode("ascii")
        headers = {
            "Authorization": authorization,
            "User-Agent": "tgfs-ff-rehearsal/1",
            "Accept": "*/*" if git else "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        if content_type:
            headers["Content-Type"] = content_type
        request = urllib.request.Request(url, data=encoded, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                payload = response.read(MAX_RESPONSE + 1)
                if len(payload) > MAX_RESPONSE:
                    raise RehearsalError("oversized response")
                return payload, {key.lower(): value for key, value in response.headers.items()}
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError) as error:
            raise RehearsalError(f"GitHub operation failed: {method} fixed endpoint") from error

    def json(self, path: str, *, method: str = "GET", body: object | None = None) -> object:
        payload, _ = self.request(f"{API}{path}", method=method, body=body)
        try:
            return json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RehearsalError("malformed JSON") from error

    def advertise(self) -> bytes:
        return self.request(
            f"{self.git_url}/info/refs?service=git-receive-pack", git=True
        )[0]

    def receive(self, body: bytes) -> bytes:
        return self.request(
            f"{self.git_url}/git-receive-pack",
            method="POST",
            body=body,
            git=True,
            content_type="application/x-git-receive-pack-request",
        )[0]


def oid(value: object) -> str:
    if not isinstance(value, str) or OID.fullmatch(value) is None:
        raise RehearsalError("invalid object id")
    return value


def pkt(payload: bytes) -> bytes:
    return f"{len(payload) + 4:04x}".encode() + payload


def parse_packets(payload: bytes) -> list[bytes]:
    lines: list[bytes] = []
    offset = 0
    while True:
        if offset + 4 > len(payload):
            raise RehearsalError("truncated packet stream")
        try:
            size = int(payload[offset : offset + 4], 16)
        except ValueError as error:
            raise RehearsalError("malformed packet length") from error
        offset += 4
        if size == 0:
            if offset != len(payload):
                raise RehearsalError("trailing packet data")
            return lines
        if size < 4 or offset + size - 4 > len(payload):
            raise RehearsalError("invalid packet size")
        lines.append(payload[offset : offset + size - 4])
        offset += size - 4


def advertised_main(payload: bytes) -> str:
    service = pkt(b"# service=git-receive-pack\n") + b"0000"
    if not payload.startswith(service):
        raise RehearsalError("missing service advertisement")
    lines = parse_packets(payload[len(service) :])
    mains = []
    for index, line in enumerate(lines):
        visible, _, capabilities = line.rstrip(b"\n").partition(b"\0")
        try:
            object_id, reference = visible.decode("ascii").split(" ", 1)
        except (UnicodeDecodeError, ValueError) as error:
            raise RehearsalError("malformed advertisement") from error
        if index == 0 and b"report-status" not in capabilities.split():
            raise RehearsalError("report-status unavailable")
        if reference == "refs/heads/main":
            mains.append(oid(object_id))
    if len(mains) != 1:
        raise RehearsalError("main advertisement ambiguity")
    return mains[0]


def empty_pack_update(old: str, new: str) -> bytes:
    command = f"{oid(old)} {oid(new)} refs/heads/main\0report-status\n".encode()
    pack = b"PACK\0\0\0\x02\0\0\0\0"
    return pkt(command) + b"0000" + pack + hashlib.sha1(pack).digest()


def receive_lines(payload: bytes) -> list[bytes]:
    if payload.endswith(b"00000000"):
        payload = payload[:-4]
    return parse_packets(payload)


def commit(client: Client, tree: str, message: str, parents: list[str]) -> str:
    identity = {
        "name": "tgfs ff rehearsal",
        "email": "ivan@relux.works",
        "date": dt.datetime.now(dt.UTC).isoformat().replace("+00:00", "Z"),
    }
    value = client.json(
        f"/repos/{ORG}/{client.repository}/git/commits",
        method="POST",
        body={"message": message, "tree": tree, "parents": parents, "author": identity, "committer": identity},
    )
    return oid(value["sha"])


def create_fixture(client: Client) -> tuple[str, str]:
    main = client.json(f"/repos/{ORG}/{client.repository}/git/ref/heads/main")
    base = oid(main["object"]["sha"])
    base_commit = client.json(f"/repos/{ORG}/{client.repository}/git/commits/{base}")
    base_tree = oid(base_commit["tree"]["sha"])
    blob2 = client.json(
        f"/repos/{ORG}/{client.repository}/git/blobs",
        method="POST",
        body={"content": "base\ncandidate\n", "encoding": "utf-8"},
    )
    tree2 = client.json(
        f"/repos/{ORG}/{client.repository}/git/trees",
        method="POST",
        body={"base_tree": base_tree, "tree": [{"path": "README.md", "mode": "100644", "type": "blob", "sha": blob2["sha"]}]},
    )
    candidate = commit(client, oid(tree2["sha"]), "candidate", [base])
    client.json(
        f"/repos/{ORG}/{client.repository}/git/refs",
        method="POST",
        body={"ref": "refs/heads/candidate", "sha": candidate},
    )
    return base, candidate


def initial_ruleset() -> dict[str, object]:
    return {
        "name": "Protect main rehearsal",
        "target": "branch",
        "enforcement": "active",
        "bypass_actors": [
            {"actor_id": None, "actor_type": "OrganizationAdmin", "bypass_mode": "pull_request"}
        ],
        "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
        "rules": [{"type": "deletion"}, {"type": "non_fast_forward"}],
    }


def desired_ruleset() -> dict[str, object]:
    value = initial_ruleset()
    value["bypass_actors"] = [
        {"actor_id": None, "actor_type": "OrganizationAdmin", "bypass_mode": "always"}
    ]
    value["rules"] = [
        {"type": "deletion"},
        {"type": "non_fast_forward"},
        {"type": "required_signatures"},
        {
            "type": "pull_request",
            "parameters": {
                "allowed_merge_methods": ["merge"],
                "dismiss_stale_reviews_on_push": True,
                "required_approving_review_count": 1,
                "require_code_owner_review": False,
                "require_extra_approval_for_unattributed_changes": True,
                "require_last_push_approval": True,
                "required_review_thread_resolution": True,
                "dismissal_restriction": {"enabled": False, "allowed_actors": []},
                "required_reviewers": [],
            },
        },
        {
            "type": "required_status_checks",
            "parameters": {
                "strict_required_status_checks_policy": True,
                "do_not_enforce_on_create": False,
                "required_status_checks": [
                    {"context": "rust-core", "integration_id": 15368},
                    {"context": "secret-scan", "integration_id": 15368},
                ],
            },
        },
    ]
    return value


def projection(value: object) -> object:
    return {key: value[key] for key in ("name", "target", "enforcement", "bypass_actors", "conditions", "rules")}


def object_set(url: str) -> set[str]:
    with tempfile.TemporaryDirectory(prefix="tgfs-ff-object-proof-") as directory:
        clone = subprocess.run(
            ["/usr/bin/git", "clone", "--quiet", "--mirror", url, directory],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if clone.returncode != 0:
            raise RehearsalError("object proof clone failed")
        result = subprocess.run(
            ["/usr/bin/git", "-C", directory, "rev-list", "--objects", "--all"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        if result.returncode != 0:
            raise RehearsalError("object proof enumeration failed")
        return {line.split(" ", 1)[0] for line in result.stdout.splitlines()}


def main() -> int:
    if len(sys.argv) != 1:
        return 64
    token = BROKER.keychain_token()
    suffix = dt.datetime.now(dt.UTC).strftime("%H%M%S")
    repository = PREFIX + suffix
    client = Client(token, repository)
    evidence: dict[str, object] = {"repository": f"{ORG}/{repository}"}
    try:
        created = client.json(
            f"/orgs/{ORG}/repos",
            method="POST",
            body={
                "name": repository,
                "description": "Disposable TASK-260830-13d48r smart-HTTP rehearsal",
                "private": False,
                "has_issues": False,
                "has_projects": False,
                "has_wiki": False,
                "allow_merge_commit": False,
                "allow_squash_merge": False,
                "allow_rebase_merge": True,
                "auto_init": True,
            },
        )
        repository_id = created["id"]
        base, candidate = create_fixture(client)
        before_repo = client.json(f"/repos/{ORG}/{repository}")
        before_settings = [
            before_repo["allow_merge_commit"],
            before_repo["allow_squash_merge"],
            before_repo["allow_rebase_merge"],
        ]
        created_ruleset = client.json(
            f"/repos/{ORG}/{repository}/rulesets", method="POST", body=initial_ruleset()
        )
        ruleset_id = created_ruleset["id"]
        rollback = projection(client.json(f"/repos/{ORG}/{repository}/rulesets/{ruleset_id}"))
        updated = client.json(
            f"/repos/{ORG}/{repository}/rulesets/{ruleset_id}",
            method="PUT",
            body=desired_ruleset(),
        )
        observed = projection(client.json(f"/repos/{ORG}/{repository}/rulesets/{ruleset_id}"))
        if projection(updated) != desired_ruleset() or observed != desired_ruleset():
            raise RehearsalError("desired ruleset round trip mismatch")
        objects_before = object_set(client.git_url)
        advertisement = advertised_main(client.advertise())
        if advertisement != base:
            raise RehearsalError("advertisement old-id mismatch")
        body = empty_pack_update(base, candidate)
        success = receive_lines(client.receive(body))
        if success != [b"unpack ok\n", b"ok refs/heads/main\n"]:
            raise RehearsalError("empty-pack update refused")
        post = oid(client.json(f"/repos/{ORG}/{repository}/git/ref/heads/main")["object"]["sha"])
        if post != candidate:
            raise RehearsalError("post-read mismatch")
        stale = receive_lines(client.receive(body))
        if stale[:1] != [b"unpack ok\n"] or not any(line.startswith(b"ng refs/heads/main ") for line in stale[1:]):
            raise RehearsalError("stale old-id was not refused")
        stale_post = oid(client.json(f"/repos/{ORG}/{repository}/git/ref/heads/main")["object"]["sha"])
        objects_after = object_set(client.git_url)
        if stale_post != candidate or objects_before != objects_after:
            raise RehearsalError("stale update moved main or object set changed")
        suite = None
        for _ in range(15):
            suites = client.json(f"/repos/{ORG}/{repository}/rulesets/rule-suites?ref=refs/heads/main")
            suite = next(
                (
                    row
                    for row in suites
                    if row.get("before_sha") == base
                    and row.get("after_sha") == candidate
                    and row.get("result") == "bypass"
                ),
                None,
            )
            if suite is not None:
                break
            time.sleep(1)
        if suite is None:
            raise RehearsalError("bypass rule suite unavailable")
        restored = client.json(
            f"/repos/{ORG}/{repository}/rulesets/{ruleset_id}", method="PUT", body=rollback
        )
        restored_observed = projection(
            client.json(f"/repos/{ORG}/{repository}/rulesets/{ruleset_id}")
        )
        after_repo = client.json(f"/repos/{ORG}/{repository}")
        after_settings = [
            after_repo["allow_merge_commit"],
            after_repo["allow_squash_merge"],
            after_repo["allow_rebase_merge"],
        ]
        final_main = oid(
            client.json(f"/repos/{ORG}/{repository}/git/ref/heads/main")["object"]["sha"]
        )
        if projection(restored) != rollback or restored_observed != rollback:
            raise RehearsalError("rollback round trip mismatch")
        if before_settings != [False, False, True] or after_settings != before_settings:
            raise RehearsalError("repository settings drift")
        if final_main != candidate:
            raise RehearsalError("rollback moved main")
        evidence.update(
            {
                "repository_id": repository_id,
                "ruleset_id": ruleset_id,
                "advertisement_equal": True,
                "empty_pack_success": True,
                "stale_old_refused": True,
                "main_exact_candidate": True,
                "git_object_set_unchanged": True,
                "bypass_rule_suite_id": suite["id"],
                "bypass_result": suite["result"],
                "ruleset_round_trip": True,
                "ruleset_rollback_exact": True,
                "repository_settings_unchanged": True,
                "rollback_forward_only": True,
                "cleanup": "repository intentionally retained for independent review",
            }
        )
        print(json.dumps(evidence, sort_keys=True))
        return 0
    except (RehearsalError, KeyError, TypeError, ValueError, OSError) as error:
        evidence["error"] = str(error)
        print(json.dumps(evidence, sort_keys=True))
        return 1
    finally:
        client.close()


if __name__ == "__main__":
    raise SystemExit(main())
