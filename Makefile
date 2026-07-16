# GramDrive repo automation. Rust workspace commands: crates/README.md.

.PHONY: build test check-arch check-licenses check-traceability check

build:
	cargo build --workspace

test:
	cargo test --workspace

check-arch:
	python3 .scripts/check_crate_architecture.py

check-licenses:
	cargo deny check licenses

check-traceability:
	python3 .scripts/validate_traceability.py

check: check-arch check-licenses check-traceability build test
