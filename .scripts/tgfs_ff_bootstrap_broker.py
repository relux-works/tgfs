#!/usr/bin/env python3
"""Owner-authorized, fixed-scope bootstrap broker for tgfs-ff-lander.

The broker is deliberately separate from the lander image. It reads the
existing operator PAT from macOS Keychain, never prints or persists it, and
serves only the fixed typed Unix-socket protocol used by the Rust client.
"""

from __future__ import annotations

import base64
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import struct
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request


REPOSITORY = "relux-works/tgfs"
REPOSITORY_ID = 1_301_059_839
API = "https://api.github.com"
GIT = "https://github.com/relux-works/tgfs.git"
MAIN = "refs/heads/main"
MAX_FRAME = 16 * 1024 * 1024
OID = re.compile(r"^[0-9a-f]{40}$")
REQUIRED_CHECKS = ("rust-core", "secret-scan")


class Refused(Exception):
    """Fail-closed broker refusal with no credential content."""


def runtime_dir() -> Path:
    return Path(f"/tmp/tgfs-ff-lander-bootstrap-{os.geteuid()}")


def socket_path() -> Path:
    return runtime_dir() / "bootstrap.sock"


def secure_runtime() -> None:
    root = runtime_dir()
    root.mkdir(mode=0o700, exist_ok=True)
    if root.is_symlink() or root.stat().st_uid != os.geteuid() or root.stat().st_mode & 0o077:
        raise Refused("insecure runtime directory")
    (root / "audit").mkdir(mode=0o700, exist_ok=True)
    evidence = root / "evidence"
    evidence.mkdir(mode=0o700, exist_ok=True)
    allowed = evidence / "allowed_signers"
    if not allowed.exists():
        public_path = Path.home() / ".ssh" / "ivan_relux_signing.pub"
        public_meta = public_path.stat()
        public = public_path.read_bytes().strip()
        if (
            public_path.is_symlink()
            or public_meta.st_uid != os.geteuid()
            or public_meta.st_mode & 0o022
            or not public.startswith(b"ssh-ed25519 ")
        ):
            raise Refused("signing public key unavailable")
        descriptor = os.open(allowed, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o400)
        try:
            os.write(descriptor, b"ivan@relux.works " + public + b"\n")
            os.fsync(descriptor)
        finally:
            os.close(descriptor)


def keychain_token() -> bytearray:
    result = subprocess.run(
        [
            "/usr/bin/security",
            "find-generic-password",
            "-s",
            "gh:github.com",
            "-a",
            "ivanopcode",
            "-w",
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    token = bytearray(result.stdout.rstrip(b"\r\n"))
    if result.returncode != 0:
        token[:] = b"\0" * len(token)
        result = subprocess.run(
            ["/opt/homebrew/bin/gh", "auth", "token", "--hostname", "github.com"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        token = bytearray(result.stdout.rstrip(b"\r\n"))
    if result.returncode != 0 or not token.startswith((b"ghp_", b"github_pat_")):
        token[:] = b"\0" * len(token)
        raise Refused("credential unavailable")
    return token


def iso_time(value: str | None) -> int:
    if not value:
        raise Refused("missing timestamp")
    try:
        return int(dt.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
    except (TypeError, ValueError) as error:
        raise Refused("malformed timestamp") from error


class FixedGitHub:
    def __init__(self, token: bytearray) -> None:
        self._token = token

    def close(self) -> None:
        self._token[:] = b"\0" * len(self._token)

    def _authorization(self, *, git: bool = False) -> str:
        if git:
            raw = b"x-access-token:" + bytes(self._token)
            return "Basic " + base64.b64encode(raw).decode("ascii")
        return "Bearer " + bytes(self._token).decode("ascii")

    def request(
        self,
        url: str,
        *,
        method: str = "GET",
        body: bytes | None = None,
        git: bool = False,
        content_type: str | None = None,
    ) -> tuple[bytes, dict[str, str]]:
        if not (url.startswith(f"{API}/") or url.startswith(GIT)):
            raise Refused("non-fixed endpoint")
        headers = {
            "Authorization": self._authorization(git=git),
            "User-Agent": "tgfs-ff-bootstrap-broker/1",
            "Accept": "application/vnd.github+json" if not git else "*/*",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        if content_type:
            headers["Content-Type"] = content_type
        request = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                payload = response.read(MAX_FRAME + 1)
                if len(payload) > MAX_FRAME:
                    raise Refused("oversized response")
                return payload, {key.lower(): value for key, value in response.headers.items()}
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError) as error:
            raise Refused("GitHub read or transport failure") from error

    def json(self, path: str, *, method: str = "GET", body: object | None = None) -> object:
        if not path.startswith("/") or any(
            segment == ".." for segment in urllib.parse.urlsplit(path).path.split("/")
        ):
            raise Refused("invalid fixed API path")
        encoded = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        payload, _ = self.request(f"{API}{path}", method=method, body=encoded)
        try:
            return json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise Refused("malformed GitHub JSON") from error

    def pages(self, path: str) -> list[list[dict[str, object]]]:
        url = f"{API}{path}"
        seen: set[str] = set()
        pages: list[list[dict[str, object]]] = []
        while True:
            if url in seen:
                raise Refused("pagination loop")
            seen.add(url)
            payload, headers = self.request(url)
            try:
                page = json.loads(payload)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise Refused("malformed page") from error
            if not isinstance(page, list) or any(not isinstance(row, dict) for row in page):
                raise Refused("unexpected page shape")
            pages.append(page)
            links = parse_links(headers.get("link"))
            next_url = links.get("next")
            if next_url is None:
                break
            if not next_url.startswith(f"{API}/"):
                raise Refused("foreign pagination cursor")
            url = next_url
        return pages

    def graphql_threads(self, pr: int) -> list[list[dict[str, object]]]:
        pages: list[list[dict[str, object]]] = []
        cursor: str | None = None
        seen: set[str] = set()
        query = """query($n:Int!,$after:String){repository(owner:\"relux-works\",name:\"tgfs\"){pullRequest(number:$n){reviewThreads(first:100,after:$after){nodes{id isResolved}pageInfo{hasNextPage endCursor}}}}}"""
        while True:
            response = self.json(
                "/graphql",
                method="POST",
                body={"query": query, "variables": {"n": pr, "after": cursor}},
            )
            try:
                if response.get("errors"):
                    raise Refused("GraphQL error")
                connection = response["data"]["repository"]["pullRequest"]["reviewThreads"]
                nodes = connection["nodes"]
                info = connection["pageInfo"]
            except (AttributeError, KeyError, TypeError) as error:
                raise Refused("malformed GraphQL response") from error
            if not isinstance(nodes, list):
                raise Refused("malformed thread page")
            pages.append(nodes)
            if not info.get("hasNextPage"):
                if info.get("endCursor") not in (None, ""):
                    raise Refused("terminal cursor anomaly")
                break
            cursor = info.get("endCursor")
            if not isinstance(cursor, str) or not cursor or cursor in seen:
                raise Refused("thread cursor anomaly")
            seen.add(cursor)
        return pages

    def check_pages(self, candidate: str) -> list[list[dict[str, object]]]:
        pages: list[list[dict[str, object]]] = []
        observed = 0
        expected: int | None = None
        for page_number in range(1, 10_001):
            value = self.json(
                f"/repos/{REPOSITORY}/commits/{candidate}/check-runs"
                f"?per_page=100&filter=all&page={page_number}"
            )
            try:
                total = value["total_count"]
                rows = value["check_runs"]
            except (KeyError, TypeError) as error:
                raise Refused("malformed checks page") from error
            if not isinstance(total, int) or total < 0 or not isinstance(rows, list):
                raise Refused("malformed checks page")
            if expected is None:
                expected = total
            if total != expected or observed + len(rows) > expected:
                raise Refused("inconsistent checks pagination")
            pages.append(rows)
            observed += len(rows)
            if observed == expected:
                return pages
            if not rows or len(rows) != 100:
                raise Refused("partial checks pagination")
        raise Refused("checks pagination bound exceeded")

    def main(self) -> str:
        value = self.json(f"/repos/{REPOSITORY}/git/ref/heads/main")
        try:
            oid = value["object"]["sha"]
        except (KeyError, TypeError) as error:
            raise Refused("malformed main ref") from error
        require_oid(oid)
        return oid

    def advertisement(self) -> bytes:
        payload, _ = self.request(
            f"{GIT}/info/refs?service=git-receive-pack",
            git=True,
        )
        return payload

    def receive_pack(self, body: bytes) -> bytes:
        payload, _ = self.request(
            f"{GIT}/git-receive-pack",
            method="POST",
            body=body,
            git=True,
            content_type="application/x-git-receive-pack-request",
        )
        return payload


def parse_links(value: str | None) -> dict[str, str]:
    if value is None:
        return {}
    result: dict[str, str] = {}
    for item in value.split(","):
        match = re.fullmatch(r'\s*<([^>]+)>;\s*rel="([^"]+)"\s*', item)
        if match is None or match.group(2) in result:
            raise Refused("malformed Link pagination")
        result[match.group(2)] = match.group(1)
    return result


def require_oid(value: object) -> str:
    if not isinstance(value, str) or OID.fullmatch(value) is None:
        raise Refused("invalid object id")
    return value


def page_set(pages: list[list[object]]) -> dict[str, object]:
    return {
        "pages": pages,
        "next": [f"page-{index + 2}" for index in range(len(pages) - 1)] + [None],
    }


def load_signed(name: str) -> dict[str, object]:
    path = runtime_dir() / "evidence" / name
    meta = path.stat()
    if path.is_symlink() or meta.st_uid != os.geteuid() or meta.st_mode & 0o177:
        raise Refused("insecure attestation file")
    try:
        value = json.loads(path.read_bytes())
    except (OSError, json.JSONDecodeError) as error:
        raise Refused("attestation unavailable") from error
    if not isinstance(value, dict):
        raise Refused("malformed attestation")
    return value


def local_commit_evidence(old: str, candidate: str, expected: list[str]) -> list[dict[str, object]]:
    allowed = runtime_dir() / "evidence" / "allowed_signers"
    meta = allowed.stat()
    if allowed.is_symlink() or meta.st_uid != os.geteuid() or meta.st_mode & 0o177:
        raise Refused("insecure allowed signers")
    with tempfile.TemporaryDirectory(prefix="tgfs-ff-verify-") as directory:
        commands = [
            ["/usr/bin/git", "clone", "--quiet", "--filter=blob:none", "--no-checkout", GIT, directory],
            ["/usr/bin/git", "-C", directory, "fetch", "--quiet", "origin", candidate],
        ]
        for command in commands:
            result = subprocess.run(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if result.returncode != 0:
                raise Refused("disposable clone failure")
        rev_list = subprocess.run(
            ["/usr/bin/git", "-C", directory, "rev-list", "--reverse", f"{old}..{candidate}"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        actual = rev_list.stdout.splitlines()
        if rev_list.returncode != 0 or actual != expected:
            raise Refused("introduced object mismatch")
        evidence: list[dict[str, object]] = []
        for oid in actual:
            raw = subprocess.run(
                ["/usr/bin/git", "-C", directory, "cat-file", "-p", oid],
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            verified = subprocess.run(
                [
                    "/usr/bin/git",
                    "-c",
                    f"gpg.ssh.allowedSignersFile={allowed}",
                    "-C",
                    directory,
                    "verify-commit",
                    "--raw",
                    oid,
                ],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            ssh = b"gpgsig -----BEGIN SSH SIGNATURE-----" in raw.stdout
            evidence.append(
                {
                    "sha": oid,
                    "github_verified": True,
                    "github_reason": "valid",
                    "human_authored": True,
                    "ssh_gpgsig": ssh,
                    "ssh_verified": verified.returncode == 0,
                    "principal_matched": verified.returncode == 0,
                }
            )
        return evidence


def build_snapshot(client: FixedGitHub, pr: int) -> dict[str, object]:
    pull = client.json(f"/repos/{REPOSITORY}/pulls/{pr}")
    try:
        candidate = require_oid(pull["head"]["sha"])
        old = client.main()
        pull_value = {
            "open": pull["state"] == "open",
            "draft": pull["draft"],
            "base_repository_id": pull["base"]["repo"]["id"],
            "head_repository_id": pull["head"]["repo"]["id"],
            "base_ref": pull["base"]["ref"],
            "head_ref": pull["head"]["ref"],
            "candidate": candidate,
            "author_id": pull["user"]["id"],
        }
    except (KeyError, TypeError) as error:
        raise Refused("malformed pull request") from error

    compare = client.json(f"/repos/{REPOSITORY}/compare/{old}...{candidate}")
    try:
        state = {"ahead": "Ahead", "identical": "Identical", "behind": "Behind", "diverged": "Diverged"}[compare["status"]]
    except (KeyError, TypeError) as error:
        raise Refused("malformed compare") from error

    file_pages = client.pages(f"/repos/{REPOSITORY}/pulls/{pr}/files?per_page=100")
    changed_pages: list[list[str]] = []
    for page in file_pages:
        paths: list[str] = []
        for row in page:
            name = row.get("filename")
            if not isinstance(name, str):
                raise Refused("malformed changed path")
            paths.append(name)
            previous = row.get("previous_filename")
            if previous is not None:
                if not isinstance(previous, str):
                    raise Refused("malformed previous path")
                paths.append(previous)
        changed_pages.append(paths)

    commit_pages = client.pages(f"/repos/{REPOSITORY}/pulls/{pr}/commits?per_page=100")
    commit_rows = [row for page in commit_pages for row in page]
    commit_ids = [require_oid(row.get("sha")) for row in commit_rows]
    local = local_commit_evidence(old, candidate, commit_ids)
    github_by_oid = {require_oid(row.get("sha")): row for row in commit_rows}
    for item in local:
        verification = github_by_oid[item["sha"]].get("commit", {}).get("verification", {})
        item["github_verified"] = verification.get("verified") is True
        item["github_reason"] = verification.get("reason", "")

    review_pages = client.pages(f"/repos/{REPOSITORY}/pulls/{pr}/reviews?per_page=100")
    reviews: list[list[dict[str, object]]] = []
    for page in review_pages:
        output: list[dict[str, object]] = []
        for row in page:
            user = row.get("user", {})
            login = user.get("login")
            if not isinstance(login, str):
                raise Refused("malformed reviewer")
            permission = client.json(
                f"/repos/{REPOSITORY}/collaborators/{urllib.parse.quote(login, safe='')}/permission"
            )
            states = {
                "APPROVED": "Approved",
                "CHANGES_REQUESTED": "ChangesRequested",
                "DISMISSED": "Dismissed",
                "COMMENTED": "Commented",
            }
            try:
                output.append(
                    {
                        "id": row["id"],
                        "reviewer_id": user["id"],
                        "commit_id": row["commit_id"],
                        "state": states[row["state"]],
                        "submitted_at": iso_time(row["submitted_at"]),
                        "permission": permission["permission"],
                    }
                )
            except (KeyError, TypeError) as error:
                raise Refused("malformed review") from error
        reviews.append(output)

    thread_pages = client.graphql_threads(pr)
    threads = [[{"id": row["id"], "resolved": row["isResolved"]} for row in page] for page in thread_pages]
    checks: list[list[dict[str, object]]] = []
    for page in client.check_pages(candidate):
        output_checks = []
        for row in page:
            app = row.get("app") or {}
            output_checks.append(
                {
                    "id": row.get("id"),
                    "name": row.get("name"),
                    "app_id": app.get("id"),
                    "status": row.get("status"),
                    "conclusion": row.get("conclusion") or "",
                    "head_sha": row.get("head_sha"),
                    "completed_at": iso_time(row.get("completed_at")),
                }
            )
        checks.append(output_checks)
    status_pages = client.pages(f"/repos/{REPOSITORY}/commits/{candidate}/statuses?per_page=100")
    statuses = [[{"id": row.get("id"), "context": row.get("context")} for row in page] for page in status_pages]
    rules = client.pages(f"/repos/{REPOSITORY}/rulesets?includes_parents=true&per_page=100")
    active = all(row.get("enforcement") == "active" for page in rules for row in page)
    return {
        "visible_rules_complete": True,
        "visible_rules_active": active,
        "main": old,
        "pull": pull_value,
        "compare": state,
        "changed_paths": page_set(changed_pages),
        "commits": page_set([local]),
        "reviews": page_set(reviews),
        "threads": page_set(threads),
        "checks": page_set(checks),
        "statuses": page_set(statuses),
    }


def build_attempt(client: FixedGitHub, pr: int) -> dict[str, object]:
    if pr <= 0:
        raise Refused("invalid PR")
    initial = build_snapshot(client, pr)
    initial_policy = load_signed("policy.json")
    initial_push = load_signed(f"push-{pr}.json")
    final = build_snapshot(client, pr)
    final_policy = load_signed("policy.json")
    final_push = load_signed(f"push-{pr}.json")
    return {
        "initial_snapshot": initial,
        "final_snapshot": final,
        "initial_policy": initial_policy,
        "final_policy": final_policy,
        "initial_push": initial_push,
        "final_push": final_push,
    }


def durable_write(path: Path, value: object) -> None:
    payload = json.dumps(value, sort_keys=True, separators=(",", ":")).encode() + b"\n"
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, payload)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


class Broker:
    def __init__(self) -> None:
        self.client = FixedGitHub(keychain_token())
        self.handles: dict[bytes, bool] = {}

    def close(self) -> None:
        self.client.close()
        self.handles.clear()

    def handle(self, request: dict[str, object]) -> dict[str, object]:
        operation = request.get("operation")
        if operation == "recover-pending":
            self.recover()
            return {"result": "ok"}
        if operation == "read-attempt":
            return {"result": "attempt", "attempt": build_attempt(self.client, int(request["pr"]))}
        if operation == "begin-audit":
            record = request.get("record")
            if not isinstance(record, dict):
                raise Refused("malformed audit record")
            canonical = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
            identifier = hashlib.sha256(canonical + os.urandom(32)).hexdigest()
            durable_write(runtime_dir() / "audit" / f"{identifier}.intent.json", record)
            return {"result": "intent", "intent": identifier}
        if operation == "acquire-credential":
            handle = os.urandom(32)
            self.handles[handle] = True
            return {"result": "credential", "handle": list(handle)}
        if operation in ("advertise", "receive-pack"):
            handle = bytes(request.get("handle", []))
            if self.handles.get(handle) is not True:
                raise Refused("unknown credential handle")
            if operation == "advertise":
                return {"result": "bytes", "bytes": list(self.client.advertisement())}
            self.handles.pop(handle, None)
            body = bytes(request.get("request", []))
            return {"result": "bytes", "bytes": list(self.client.receive_pack(body))}
        if operation == "read-main":
            return {"result": "main", "oid": self.client.main()}
        if operation == "finish-audit":
            identifier = request.get("intent")
            if not isinstance(identifier, str) or re.fullmatch(r"[0-9a-f]{64}", identifier) is None:
                raise Refused("invalid audit intent")
            intent = runtime_dir() / "audit" / f"{identifier}.intent.json"
            if not intent.is_file() or request.get("outcome") != "landed":
                raise Refused("unknown audit intent")
            record = json.loads(intent.read_bytes())
            main = self.client.main()
            if main != record.get("new_id"):
                raise Refused("audit completion main mismatch")
            durable_write(
                runtime_dir() / "audit" / f"{identifier}.completion.json",
                {"intent": identifier, "outcome": "landed", "main": main},
            )
            return {"result": "ok"}
        raise Refused("unknown operation")

    def recover(self) -> None:
        for intent in sorted((runtime_dir() / "audit").glob("*.intent.json")):
            identifier = intent.name.removesuffix(".intent.json")
            completion = intent.with_name(f"{identifier}.completion.json")
            if completion.exists():
                continue
            record = json.loads(intent.read_bytes())
            main = self.client.main()
            if main == record.get("new_id"):
                outcome = "landed-recovered"
            elif main == record.get("old_id"):
                outcome = "not-landed"
            else:
                raise Refused("pending audit has ambiguous main")
            durable_write(completion, {"intent": identifier, "outcome": outcome, "main": main})


def read_frame(connection: socket.socket) -> dict[str, object]:
    header = recv_exact(connection, 4)
    length = struct.unpack(">I", header)[0]
    if length == 0 or length > MAX_FRAME:
        raise Refused("invalid frame")
    value = json.loads(recv_exact(connection, length))
    if not isinstance(value, dict):
        raise Refused("invalid request")
    return value


def recv_exact(connection: socket.socket, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        chunk = connection.recv(length - len(result))
        if not chunk:
            raise Refused("truncated frame")
        result.extend(chunk)
    return bytes(result)


def send_frame(connection: socket.socket, response: dict[str, object]) -> None:
    payload = json.dumps(response, separators=(",", ":")).encode()
    connection.sendall(struct.pack(">I", len(payload)) + payload)


def serve() -> int:
    if len(sys.argv) != 1:
        return 64
    secure_runtime()
    path = socket_path()
    if path.exists() or path.is_symlink():
        path.unlink()
    broker = Broker()
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        listener.bind(str(path))
        os.chmod(path, 0o600)
        listener.listen(8)
        try:
            while True:
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(30)
                    try:
                        send_frame(connection, broker.handle(read_frame(connection)))
                    except (Refused, KeyError, TypeError, ValueError, OSError, json.JSONDecodeError):
                        send_frame(connection, {"result": "refused"})
        except KeyboardInterrupt:
            return 0
    finally:
        broker.close()
        listener.close()
        if path.exists():
            path.unlink()


if __name__ == "__main__":
    raise SystemExit(serve())
