# GramDrive repo automation.
#
# Every gate runs through one entrypoint — .scripts/acceptance/run_automated.py —
# which is the same script CI invokes. These targets are shorthand for it, never
# a second copy of the commands: a gate defined in two places is two gates that
# disagree the moment one of them is edited. `make gates` prints the suites and
# the exact command behind every step.
#
# Rust workspace layout and rules: crates/README.md.

GATE := python3 .scripts/acceptance/run_automated.py

.PHONY: check check-core check-repo gates fmt build test clean-gates

# check — the pre-push gate: everything CI runs.
check:
	$(GATE) --suite all --run-id local-all

# check-core — Rust core: toolchain, format, lint, test, architecture, supply chain.
check-core:
	$(GATE) --suite core --run-id local-core

# check-repo — docs and tooling: traceability, script self-tests.
check-repo:
	$(GATE) --suite repo --run-id local-repo

# gates — list the suites and the exact command each step runs.
gates:
	$(GATE) --list

# --- Developer conveniences (not gates) --------------------------------------
# The fixers and the inner loop. Note the difference from the gate targets:
# `make fmt` rewrites files, the gate's format step only checks; `make test`
# skips provenance so it stays fast to re-run.

# fmt — apply rustfmt across the workspace.
fmt:
	cargo fmt --all

# build — compile every crate.
build:
	cargo build --workspace

# test — run the workspace tests directly (fast inner loop, no provenance).
test:
	cargo test --workspace

# clean-gates — drop local gate provenance under .temp/acceptance/.
clean-gates:
	rm -rf .temp/acceptance
