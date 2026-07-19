# TASK-260719-1dwaj8 — Review verdict: ACCEPTED → done

Read-only review, 2026-07-20. Every AC re-verified independently (GitHub API, gh run logs, ssh relux read-only, local actionlint) — not taken from the results resource on faith.

## AC verification

| AC | Verdict | Independent evidence |
|---|---|---|
| Runner online, launchd, label gramdrive-mac | PASS | GitHub API: relux-gramdrive online, labels self-hosted/macOS/X64/gramdrive-mac; ssh relux: LaunchAgent actions.runner.relux-works-tgfs.relux-gramdrive loaded (pid 17646, status 0). Restart proof (svc stop/start → online) from dev, consistent with observed state. |
| Toolchain versions recorded | PASS | Results resource carries full pinned+checksummed table (Xcode 26.2 universal via DEVELOPER_DIR, rustup 1.91.0 + aarch64-apple-darwin, python 3.12.13, cmake 4.3.3, gitleaks 8.30.1, gperf 3.0.3); TDLib path documented: cache-seeded (cross-build blocked by x86_64-only brew OpenSSL, exactly the fallback the task pre-authorized). Xcode_26_5 rsync fallback correctly declared dead (arm64-only, minOS 26.2) and resolved without stop-the-line via pre-existing universal Xcode 26.2. |
| ci + native-ci green on runner, arm64 via file(1) | PASS | gh run view: CI 29702010556 (d46b203) + 29702440710 (HEAD 99ad6a9), native-ci 29702010606 (d46b203) + 29702440760 (HEAD) — all success, all jobs on relux-gramdrive. file(1) lines extracted from run 29702010606 log: libtdjson.dylib Mach-O arm64; gramdrive-agent / GramDrive / GramDriveFileProvider all Mach-O 64-bit executable arm64. x86_64 deviation documented in job comments (ci.yml rust-core, native-ci apple-build-test). |
| release.yml on runner, temp-keychain + always()-cleanup intact | PASS | release.yml runs-on [self-hosted, gramdrive-mac]; throwaway keychain in RUNNER_TEMP; always() cleanup deletes it, restores default keychain + search list VERBATIM (fix 99ad6a9 closes the measured delete-keychain dangling-entry anomaly), wipes .temp after artifact upload. Sim-verified on relux (dummy-p12 lifecycle). Actual tag run needs owner re-tag (v0.1.0 pre-dates migration; tag-triggered workflows run the file at the tag commit) + POL-8 human gate — correctly documented as owner action. |
| No residual secrets on relux | PASS | Measured post-runs via ssh: keychain search list + default = login.keychain-db only; no *gramdrive* in ~/Library/Keychains; no .p12/.p8/.keychain-db under ~/actions-runner or ~/gramdrive-ci. |
| Lint / tests / build | PASS | actionlint re-run locally on all 3 workflows → exit 0 (with .github/actionlint.yaml declaring the label). CI + native-ci green at HEAD exercise the full acceptance suites incl. the +9 new script tests. |

## Architecture fit

One-entrypoint contract preserved on the new runner (every gate still run_automated.py --suite X); actions pinned by SHA; tool downloads pinned + checksum-verified; least-privilege permissions unchanged; honest documented deviations instead of silent ones. Fits the barycenter pattern the story established.

## Non-blocking notes (no changes requested)

1. release.yml:115 still carries Swatinem/rust-cache while ci.yml/native-ci dropped hosted-cache steps as billing-blocked. rust-cache degrades to a warning on cache-service failure, so a release run will not break — but it is dead weight and inconsistent with the stated decision. Drop on the next release.yml touch.
2. Physical reboot of relux not exercised (box hosts unrelated live services — coolify, tundra-relay, market-impulse; not this task
s call). AC required service restart, which was proven. LaunchAgent + auto-login session documented as the reboot story; residual risk accepted and visible.
3. LOGBOOK.md review entry 0036 added by this review — uncommitted, for the owner to include in the next commit (reviewer does not commit).
