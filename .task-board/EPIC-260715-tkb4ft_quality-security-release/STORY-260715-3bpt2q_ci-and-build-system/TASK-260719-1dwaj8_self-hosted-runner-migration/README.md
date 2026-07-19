# TASK-260719-1dwaj8: self-hosted-runner-migration

## Description
Provision a self-hosted GitHub Actions runner on the spare Intel Mac (ssh relux, x86_64, macOS 15.7, bare) and migrate ci/native-ci/release workflows to it, producing arm64 artifacts via cross-compilation. Motivation: GitHub-hosted macOS minutes blocked by org billing; the release run for v0.1.0 (run 29699941040) cannot start. Toolchain delivery: try Command Line Tools first (softwareupdate; check swift build, xcrun notarytool, xcrun stapler availability); fall back to rsync-copying /Applications/Xcode_26_5.app from this host (verify it launches on macOS 15.7 Intel — if incompatible, document and stop-the-line). Rust via rustup with aarch64-apple-darwin target; cmake/gperf/gitleaks via brew (install brew) or direct binaries. TDLib artifact: seed the pinned build cache from this host via rsync if cross-building arm64 TDLib on Intel is blocked by arm64 OpenSSL deps — record which path was taken. Runner: register via gh api registration-token as a repo runner with custom label gramdrive-mac, install as launchd service (./svc.sh install), survives reboot. Workflows: switch runs-on to [self-hosted, gramdrive-mac] for macOS jobs (secret-scan job may also move or stay ubuntu — decide and document); preserve the temp-keychain isolation and always()-cleanup in release.yml — CRITICAL on a persistent runner; add workspace hygiene where the ephemeral-runner assumption breaks. Verify: cross-built binaries are arm64 per file(1); CI green on the runner.

## Scope
(define task scope)

## Acceptance Criteria
Runner online as a launchd service with label gramdrive-mac and survives service restart; toolchain versions recorded in the results resource; ci and native-ci green on the self-hosted runner with arm64 binaries proven via file(1); release.yml runs on the runner with temp-keychain isolation and always()-cleanup intact; no residual secrets or credentials on relux outside a run lifetime.
