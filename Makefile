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

.PHONY: check check-core check-repo check-security check-apple gates fmt build test bindings \
        smoke-bindings smoke-shared-state smoke-agent-lifecycle package \
        package-reproducible package-app package-app-unsigned package-app-notarize \
        tdlib tdlib-smoke tdjson-smoke tdlib-verify clean-gates

# check — the pre-push gate: the core and repo suites (what CI's rust-core job
# runs). Secret scanning is a separate suite (make check-security) because it
# needs gitleaks; CI runs it as its own required job.
check:
	$(GATE) --suite all --run-id local-all

# check-core — Rust core: toolchain, format, lint, test, architecture, supply chain.
check-core:
	$(GATE) --suite core --run-id local-core

# check-repo — docs and tooling: traceability, script self-tests.
check-repo:
	$(GATE) --suite repo --run-id local-repo

# check-security — gitleaks secret scan of committed history (needs gitleaks:
# `brew install gitleaks`). Run as its own required CI job (secret-scan).
check-security:
	$(GATE) --suite security --run-id local-security

# check-apple — the macOS native leg: swift build + swift test of
# apple/GramDriveSupport against the staged core. Needs Xcode and a prior
# `make package` (the Swift package resolves GramDriveCore by path). Its own
# suite, kept out of `make check` because that must run without Xcode or the
# staged core; native-ci runs it as its own job. Same entrypoint as CI.
check-apple:
	$(GATE) --suite apple --run-id local-apple

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

# smoke-shared-state — the multi-process shared-container proof: a Rust
# coordinator process seeds, two concurrent Swift provider processes (the
# apple/GramDriveSupport package over the packaged artifact) must read
# identical item metadata, and a watcher process must observe the change
# doorbell plus the data-version probe across a foreign commit. Needs Xcode;
# stages `make package` if no artifact is present
# (see .scripts/smoke/run_shared_state_smoke.py).
smoke-shared-state:
	python3 .scripts/smoke/run_shared_state_smoke.py

# smoke-agent-lifecycle — the companion agent as real processes: startup with
# health over the bounded IPC channel, single-instance refusal of a second
# agent, SIGTERM drain (hosted transfer cancelled through its token, exit 0,
# endpoint torn down), and instant successor startup after SIGKILL. Needs
# Xcode; stages `make package` if no artifact is present
# (see .scripts/smoke/run_agent_lifecycle_smoke.py).
smoke-agent-lifecycle:
	python3 .scripts/smoke/run_agent_lifecycle_smoke.py

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

# --- macOS app packaging -----------------------------------------------------
# The signed, notarizable GramDrive.app: the menu-bar shell, the launchd agent,
# and the File Provider extension appex assembled into one Developer ID bundle
# and its dmg (TASK-260715-1dk9ik). Same posture as `package`/`tdlib` above —
# needs Xcode, a signing identity, and the staged core (`make package` first),
# so it is not a step of `check`; CI runs it as its own job. Like those, it is a
# reusable script the release workflow (TASK-260715-3bhbkv) invokes, never a
# second copy of the codesign/notarytool commands. The script writes a
# purpose-built provenance record (manifest.json: identity, entitlements,
# cdhashes, checksums; NFR-052), so it runs directly, not through $(GATE).
# Pipeline and layout: .scripts/apple-app/README.md.

# package-app — build, assemble, and sign GramDrive.app + dmg, then verify
# (codesign --deep --strict, entitlement dump, Gatekeeper). No notarization.
# Output: .temp/app-packaging/.
package-app:
	python3 .scripts/apple-app/build_app_bundle.py

# package-app-unsigned — the assembly gate: build and lay out GramDrive.app and
# its plists, then stop before codesign (no Developer ID, no dmg, no
# notarization). This is what native-ci runs on an ordinary runner to prove the
# packaging/assembly contract without a signing identity. Output: .temp/app-packaging/.
package-app-unsigned:
	python3 .scripts/apple-app/build_app_bundle.py --unsigned

# package-app-notarize — the full release path: package-app, then submit the dmg
# to Apple via the gramdrive-notary keychain profile, wait, staple, and re-check
# Gatekeeper. Needs network and the notary profile / ASC secrets.
package-app-notarize:
	python3 .scripts/apple-app/build_app_bundle.py --notarize

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
