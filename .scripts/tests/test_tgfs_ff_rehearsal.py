from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest


SCRIPT = Path(__file__).parents[1] / "tgfs_ff_rehearsal.py"
SPEC = importlib.util.spec_from_file_location("tgfs_ff_rehearsal", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
rehearsal = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = rehearsal
SPEC.loader.exec_module(rehearsal)


class ProtocolTests(unittest.TestCase):
    def test_empty_pack_has_one_fixed_main_command_and_no_options(self) -> None:
        old = "1" * 40
        new = "2" * 40
        body = rehearsal.empty_pack_update(old, new)
        command_end = body.index(b"0000") + 4
        command = rehearsal.parse_packets(body[:command_end])
        self.assertEqual(
            command,
            [f"{old} {new} refs/heads/main\0report-status\n".encode()],
        )
        self.assertNotIn(b"push-option", body)
        self.assertNotIn(b"force", body)
        self.assertEqual(body[command_end : command_end + 12], b"PACK\0\0\0\x02\0\0\0\0")

    def test_receive_accepts_github_terminal_flush_only(self) -> None:
        status = (
            rehearsal.pkt(b"unpack ok\n")
            + rehearsal.pkt(b"ok refs/heads/main\n")
            + b"00000000"
        )
        self.assertEqual(
            rehearsal.receive_lines(status),
            [b"unpack ok\n", b"ok refs/heads/main\n"],
        )
        with self.assertRaises(rehearsal.RehearsalError):
            rehearsal.receive_lines(status + b"0000")

    def test_ruleset_exactly_carries_response_normalized_field(self) -> None:
        pull = next(
            rule for rule in rehearsal.desired_ruleset()["rules"] if rule["type"] == "pull_request"
        )
        self.assertIs(
            pull["parameters"]["require_extra_approval_for_unattributed_changes"], True
        )
        self.assertEqual(
            rehearsal.desired_ruleset()["bypass_actors"],
            [
                {
                    "actor_id": None,
                    "actor_type": "OrganizationAdmin",
                    "bypass_mode": "always",
                }
            ],
        )


if __name__ == "__main__":
    unittest.main()
