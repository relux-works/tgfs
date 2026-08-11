# GramDrive update operations runbook

This public runbook is the operator contract for Sparkle update delivery. Never put credential values, private-key material, key fingerprints, certificates, runtime Telegram inputs, or decoded files in source, shell history, logs, prompts, board resources, Actions artifacts, releases, or Pages. Enter values only through GitHub environment-secret UI or approved secret automation that writes directly to GitHub.

## One-time bootstrap

1. Create `updates-test`, restricted to protected `main`, with no approval rule. Create `release`, restricted to validated `v*` tags, with an owner reviewer and no self-review/bypass where GitHub supports it. Enable Pages from GitHub Actions.
2. Provision a dedicated runner account and label. Permit only trusted signing workflows; block untrusted concurrent jobs, interactive access, unrelated processes, and untrusted runner administration while signing runs. Treat workflow and runner-admin changes as signing-key access.
3. Independently generate test and stable Sparkle keypairs offline. Commit only reviewed public keys and versioned feed URLs. Store each private export only in encrypted offline escrow and its designated environment secret—never share a key or copy it between environments.
4. Add exactly the seven initial names below using GitHub's secret UI or approved automation. Then run the value-free preflight:

   ```sh
   python3 .scripts/release/check_update_secret_inventory.py --check-github
   ```

5. A `release` owner approves stable promotion only after the candidate's exact checksum, commit, build, Apple verification, test-feed verification, and second-Mac evidence are recorded.

The preflight calls only `gh secret list --env ...`, compares names, and returns non-zero for a missing or unexpected name. It has no value argument and never calls `gh secret set` or reads values.

## Initial secret inventory

| Environment | Name | Purpose and lifecycle |
| --- | --- | --- |
| `updates-test` | `MACOS_CERT_P12` | Developer ID Application export; decode only under `RUNNER_TEMP`, import into a throwaway keychain. |
| `updates-test` | `MACOS_CERT_PASSWORD` | Export password; rotate with every export. |
| `updates-test` | `APPSTORE_KEY_ID` | App Store Connect API key identifier for `notarytool`. |
| `updates-test` | `APPSTORE_ISSUER_ID` | App Store Connect issuer identifier for `notarytool`. |
| `updates-test` | `APPSTORE_PRIVATE_KEY` | API `.p8` content; write only under `RUNNER_TEMP` long enough to create the throwaway-keychain profile. |
| `updates-test` | `SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64` | Test feed/enclosure EdDSA signing key. |
| `release` | `SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64` | Stable feed/enclosure EdDSA signing key; unavailable before approval. |

`GITHUB_TOKEN` is job-scoped, not bootstrap inventory. Public Sparkle keys, feed URLs, identity/team name, bundle IDs, and marketing version are reviewed configuration. `GRAMDRIVE_API_ID`, `GRAMDRIVE_API_HASH`, Telegram authorization, and user data are local runtime inputs and never release automation secrets.

New generations are named `SPARKLE_TEST_V<N>_EDDSA_PRIVATE_KEY_B64` and `SPARKLE_STABLE_V<N>_EDDSA_PRIVATE_KEY_B64`. During a stable bridge retain the old stable generation in `release`; after freezing legacy bytes, remove it from CI and keep or destroy offline escrow only under the support/recovery decision.

## Temporary `.p12` argv exposure

`security import -P` briefly exposes `MACOS_CERT_PASSWORD` in process argv. GitHub masking and disabled tracing do not remove the host-level risk. Mandatory temporary mitigations are the dedicated-runner controls above, generated throwaway keychain passwords, decoded files only under `RUNNER_TEMP`, no `set -x`, no literal arguments, no credential-bearing artifacts, and `always()` deletion of decoded files, keychain state, and workspace secrets. Inspect only identity names, expiry, and privacy-safe verification results.

After this iteration, issue a replacement Developer ID certificate/export, validate a bridge release, remove the temporary export from GitHub, runner, and operator devices, then revoke the temporary certificate. On suspected compromise, freeze first and revoke with Apple immediately.

## Independent rotations and revocation

Never rotate Developer ID and a Sparkle trust key in one bridge build.

### Stable Sparkle V1 to V2

1. Generate and escrow V2; add `SPARKLE_STABLE_V2_EDDSA_PRIVATE_KEY_B64` only to `release`. Review its public key and `https://relux-works.github.io/tgfs/updates/stable/v2/stable.xml`. Do not change Developer ID.
2. Build a higher-build stable bridge embedding the V2 key/URL. Publish exact bytes through test and validate appcast signature plus installed bundle configuration.
3. With release approval, use V1 to publish the final `.../stable/v1/stable.xml` whose highest item is the bridge and whose enclosure is V1-signed. In the same complete Pages site publish V2 signed by V2. Never sign V1's URL with V2.
4. Test a previously installed V1 stable client before publication, after it sees the bridge, after relaunch onto V2, and after a later V2-only update. Record only build and feed generation.
5. Cut new downloads/releases to V2 only after that passes. Keep the V1 URL, old-key-signed bridge feed, bridge DMG, checksums, and release bytes live for supported clients. Freeze exact legacy bytes as checksummed immutable release assets.
6. Remove V1 from CI only after freeze. Retain escrow through the support/recovery window; destroy it only with a two-person record of generation, custodians, time, and authorization—never the value.

Test-key rotation follows the same pattern: V1 `updates-test-v1/test.xml` signed by V1 offers a bridge embedding V2 and `updates-test-v2/test.xml`; keep V1 live for supported test clients. If every test client is disposable, revocation plus a manual reinstall is permitted only when that discontinuity is recorded.

### Developer ID certificate/export

Issue a replacement first; leave both Sparkle keys unchanged. Build a higher stable candidate using the new identity, validate through test on a second Mac, and promote those exact bytes with the current stable key. Only then remove old exports and revoke the temporary certificate. Confirmed compromise requires immediate freeze and Apple revocation, even if manual reinstall is necessary.

### App Store Connect notary key

Create a new API key, replace `APPSTORE_KEY_ID`, `APPSTORE_ISSUER_ID`, and `APPSTORE_PRIVATE_KEY` together in `updates-test`, prove notarization and privacy-safe log review using the new profile, then revoke the old key. It changes neither application nor Sparkle trust and rotates independently.

## Emergency freeze and forward recovery

1. Freeze the affected workflow/environment and block runner access. Preserve privacy-safe run, release, and deployment evidence only.
2. Restore the last known-good signed feed/site by redeploying the complete checksummed Pages artifact stored with that stable release. Failed Pages deployment leaves the prior site live; failed test-feed replacement restores validated `test.xml` and may briefly return retryable 404.
3. Withdraw a bad item with a higher-integrity signed feed that omits it. Never mutate a released DMG; keep frozen legacy feeds live.
4. A hostile runner compromises every credential and artifact it could access. Test-key compromise cannot authorize stable updates; rotate test keys or require reinstall. Stable-key or Developer ID compromise needs new credentials and a bridge only if the trust path remains safe.
5. Roll back forward only: rebuild last-known-good source as a new protected-main commit with greater `CFBundleVersion`, sign/notarize, validate through test, then promote. Sparkle will not automatically downgrade. For unsafe stable code, remove it from the signed feed, freeze, revoke affected credentials/tickets, and offer a clean notarized manual DMG while recovering the bridge.

Stable promotion must verify source commit, build, checksum, bundle/team identity, Apple signature, notarization, staple, Gatekeeper, and test-feed EdDSA signature before exact-byte publication. Retain a checksummed complete Pages artifact per release. Old-client bridge tests, legacy-feed retention, and post-iteration Developer ID reissue remain mandatory.
