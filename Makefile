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

.PHONY: check check-core check-repo gates fmt build test bindings smoke-bindings \
        package package-reproducible tdlib tdlib-smoke tdjson-smoke tdlib-verify \
        clean-gates

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

# bindings — generate Swift + Kotlin bindings from the built library
# (library mode; pipeline documented in crates/gramdrive-ffi/README.md).
bindings:
	cargo build -p gramdrive-ffi
	cargo run -p gramdrive-ffi --features bindgen --bin uniffi-bindgen -- \
		generate --library target/debug/libgramdrive_ffi.dylib \
		--language swift --language kotlin --out-dir .temp/bindings

# smoke-bindings — build, generate, then compile and run the Swift and
# Kotlin smoke consumers against the generated bindings (needs swiftc,
# kotlinc, java; see .scripts/smoke/run_bindings_smoke.py).
smoke-bindings:
	python3 .scripts/smoke/run_bindings_smoke.py

# --- Artifact packaging ------------------------------------------------------
# Not gate targets, and deliberately not steps of `check`: they need Xcode and a
# release build, and they produce artifacts rather than a pass/fail on the
# source. Same reasoning as smoke-bindings above; CI runs them as their own job.
#
# These invoke the packaging script directly rather than through $(GATE). The
# gate entrypoint exists to give a check run attributable provenance, and the
# packaging script already writes a stronger, purpose-built record of exactly
# that kind — manifest.json with commit, toolchain versions, sizes and
# checksums (NFR-052). Routing it through the gate would add a second, weaker
# provenance record of the same run, which is the drift the one-entrypoint rule
# is there to prevent.

# package — build the artifacts native consumers ship against: the XCFramework,
# the generated Swift bindings, the manifest and the checksums, then prove them
# by resolving and running a real minimal Swift package against the result.
# Output: .temp/packaging/ (pipeline and layout: .scripts/packaging/README.md).
package:
	python3 .scripts/packaging/build_core_artifacts.py

# package-reproducible — build the shipped library at two different paths from
# clean and compare bytes. The manifest's path_independent claim is only worth
# the check behind it, and a check that does not vary the path cannot falsify it.
package-reproducible:
	python3 .scripts/packaging/build_core_artifacts.py --check-reproducible

# --- TDLib artifact ----------------------------------------------------------
# The pinned tdjson library GramDrive's local Telegram source links against.
# Not a gate target and deliberately not a step of `check`: it needs a macOS
# arm64 host with Xcode, cmake, gperf and OpenSSL, and a from-source C++ build,
# and it produces an artifact rather than a pass/fail on the source. Same
# reasoning as `package` above; CI runs it as its own job. Like `package`, it
# invokes the script directly rather than through $(GATE) — the script writes a
# stronger, purpose-built provenance record (manifest.json: pin, toolchain,
# checksums; NFR-052), and routing it through the gate would add a second,
# weaker record of the same run. The faked-subprocess self-tests DO run in the
# `repo` gate suite, so `make check` covers the pipeline without Xcode or a
# network. Pipeline and layout: .scripts/tdlib/README.md.

# tdlib — fetch the pinned TDLib, build libtdjson.dylib + headers, stage the
# manifest and checksums, and prove it with the Rust link smoke. Output:
# .temp/tdlib/out/.
tdlib:
	python3 .scripts/tdlib/build_tdlib.py

# tdlib-smoke — re-run only the Rust link smoke against an already-staged
# artifact (links libtdjson, calls the C JSON interface, prints the version).
tdlib-smoke:
	GRAMDRIVE_TDLIB_ARTIFACT_DIR="$(CURDIR)/.temp/tdlib/out" \
		cargo run --quiet --release --manifest-path .scripts/tdlib/link-smoke/Cargo.toml

# tdjson-smoke — run the gramdrive-source-tdjson wrapper's real-linkage smoke
# against the staged artifact. The env variable is the gate: with it set, the
# crate's build.rs enables cfg(real_tdjson), links libtdjson.dylib and bakes
# in its rpath, and the otherwise-empty real_tdjson_smoke test binary runs
# the actual runtime against the actual library. Without it (every `make
# check`), the crate builds mock-only and this test compiles to nothing.
tdjson-smoke:
	GRAMDRIVE_TDLIB_ARTIFACT_DIR="$(CURDIR)/.temp/tdlib/out" \
		cargo test -p gramdrive-source-tdjson --test real_tdjson_smoke

# tdlib-verify — build the library twice from a clean build tree and compare
# bytes. The manifest's path_independent claim is only worth the check behind
# it; same-machine reproducibility is what CI caching depends on.
tdlib-verify:
	python3 .scripts/tdlib/build_tdlib.py --verify

# clean-gates — drop local gate provenance under .temp/acceptance/.
clean-gates:
	rm -rf .temp/acceptance
