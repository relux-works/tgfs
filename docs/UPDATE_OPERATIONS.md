# GramDrive update operations runbook

This public runbook is the operator contract for Sparkle update delivery. Never put credential values, private-key material, key fingerprints, certificates, runtime Telegram inputs, or decoded files in source, shell history, logs, prompts, board resources, Actions artifacts, releases, or Pages. The repository setter accepts values only on standard input or from owner-only files and suppresses `gh` output; it never puts a value in an argument or prints one.

## One-time bootstrap

1. Create `updates-test`, restricted to protected `main`, with no approval rule. Create `release`, restricted to validated `v*` tags, with an owner reviewer and no self-review/bypass where GitHub supports it. Enable Pages from GitHub Actions.
2. Provision a dedicated runner account and label. Permit only trusted signing workflows; block untrusted concurrent jobs, interactive access, unrelated processes, and untrusted runner administration while signing runs. Treat workflow and runner-admin changes as signing-key access.
3. Use a local operator machine and a non-synchronised, owner-only staging directory. Its directory mode must be `0700` and each named input file mode `0600`; obtain the bytes out of band and never type them into a terminal. The file contents are, respectively, the base64 Developer ID `.p12`, its export password, and the three replacement notary fields. Do not leave this directory on the runner.
4. Store the five Apple credentials with the repository helper. It validates the permissions before reading each file, streams each value to `gh secret set NAME --env updates-test` on stdin, and prints only public names:

   ```sh
   python3 .scripts/release/check_update_secret_inventory.py --set-developer-id-from "$SECURE_INPUT_DIR"
   python3 .scripts/release/check_update_secret_inventory.py --set-notary-from "$SECURE_INPUT_DIR"
   ```

5. Independently generate test and stable Sparkle keypairs offline. For each generation, run Sparkle's `generate_keys -x`, place its private export in encrypted offline escrow, review and commit only the public key/feed URL, then stream the base64 export directly into its designated environment secret. The pipelines must have no diagnostic consumer and must not be run with shell tracing:

   ```sh
   generate_keys -x | base64 | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64
   generate_keys -x | base64 | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64
   ```

   Run the two commands in separate offline key-generation sessions; never reuse or copy an export between `updates-test` and `release`.

6. Then run the value-free preflight:

   ```sh
   python3 .scripts/release/check_update_secret_inventory.py --check-github
   ```

7. A `release` owner approves stable promotion only after the candidate's exact checksum, commit, build, Apple verification, test-feed verification, and second-Mac evidence are recorded.

After storage, remove the staging files and directory using the organisation's approved local destruction procedure, then record only generation, custodian, authorization, and time for offline escrow. The preflight calls only `gh secret list --env ...`, compares names, and returns non-zero for a missing or unexpected name.

## Exact value-safe setter interface

`check_update_secret_inventory.py --set NAME` accepts one allowed initial or versioned Sparkle name and reads its non-empty bytes from stdin. It invokes exactly `gh secret set NAME --env ENV`, with no `--body` argument, and discards `gh` stdout/stderr. `SPARKLE_TEST_V<N>_EDDSA_PRIVATE_KEY_B64` maps only to `updates-test`; `SPARKLE_STABLE_V<N>_EDDSA_PRIVATE_KEY_B64` maps only to `release`.

`--set-developer-id-from DIRECTORY` requires owner-only `MACOS_CERT_P12` and `MACOS_CERT_PASSWORD` files and replaces that pair in `updates-test`. `--set-notary-from DIRECTORY` requires owner-only `APPSTORE_KEY_ID`, `APPSTORE_ISSUER_ID`, and `APPSTORE_PRIVATE_KEY` files and replaces all three in `updates-test` as one operator change. The helper validates every group input before its first write. If a GitHub write fails, leave the old credential revocation pending, rerun the same grouped command after correcting access, and use `--check-github` to confirm the public-name inventory. It never reads remote secret values.

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

1. Generate and escrow V2 in a separate offline session, then set it only in `release` without an argv value:

   ```sh
   generate_keys -x | base64 | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_STABLE_V2_EDDSA_PRIVATE_KEY_B64
   ```

   Review its public key and `https://relux-works.github.io/tgfs/updates/stable/v2/stable.xml`. Do not change Developer ID.
2. Build a higher-build stable bridge embedding the V2 key/URL. Publish exact bytes through test and validate appcast signature plus installed bundle configuration.
3. With release approval, use V1 to publish the final `.../stable/v1/stable.xml` whose highest item is the bridge and whose enclosure is V1-signed. In the same complete Pages site publish V2 signed by V2. Never sign V1's URL with V2.
4. Test a previously installed V1 stable client before publication, after it sees the bridge, after relaunch onto V2, and after a later V2-only update. Record only build and feed generation.
5. Cut new downloads/releases to V2 only after that passes. Keep the V1 URL, old-key-signed bridge feed, bridge DMG, checksums, and release bytes live for supported clients. Freeze exact legacy bytes as checksummed immutable release assets.
6. Remove V1 from CI only after freeze. Retain escrow through the support/recovery window; destroy it only with a two-person record of generation, custodians, time, and authorization—never the value.

Test-key rotation follows the same pattern: create the independent V2 export and stream it only to `updates-test` with `--set SPARKLE_TEST_V2_EDDSA_PRIVATE_KEY_B64`; V1 `updates-test-v1/test.xml` signed by V1 offers a bridge embedding V2 and `updates-test-v2/test.xml`; keep V1 live for supported test clients. If every test client is disposable, revocation plus a manual reinstall is permitted only when that discontinuity is recorded.

### Developer ID certificate/export

Issue a replacement first; leave both Sparkle keys unchanged. Put the new base64 `.p12` and its password in a new owner-only staging directory and run `--set-developer-id-from "$SECURE_INPUT_DIR"`. Build a higher stable candidate using the new identity, validate through test on a second Mac, and promote those exact bytes with the current stable key. Only then destroy the staging input, remove old exports, and revoke the temporary certificate. Confirmed compromise requires immediate freeze and Apple revocation, even if manual reinstall is necessary.

### App Store Connect notary key

Create a new API key and put its three fields in a new owner-only staging directory. Run `--set-notary-from "$SECURE_INPUT_DIR"` to replace `APPSTORE_KEY_ID`, `APPSTORE_ISSUER_ID`, and `APPSTORE_PRIVATE_KEY` together in `updates-test`, prove notarization and privacy-safe log review using the new profile, then destroy the staging input and revoke the old key. It changes neither application nor Sparkle trust and rotates independently.

## Emergency freeze and forward recovery

1. Freeze the affected workflow/environment and block runner access. Preserve privacy-safe run, release, and deployment evidence only.
2. Restore the last known-good signed feed/site by redeploying the complete checksummed Pages artifact stored with that stable release. Failed Pages deployment leaves the prior site live; failed test-feed replacement restores validated `test.xml` and may briefly return retryable 404.
3. Withdraw a bad item with a higher-integrity signed feed that omits it. Never mutate a released DMG; keep frozen legacy feeds live.
4. A hostile runner compromises every credential and artifact it could access. Test-key compromise cannot authorize stable updates; rotate test keys or require reinstall. Stable-key or Developer ID compromise needs new credentials and a bridge only if the trust path remains safe.
5. Roll back forward only: rebuild last-known-good source as a new protected-main commit with greater `CFBundleVersion`, sign/notarize, validate through test, then promote. Sparkle will not automatically downgrade. For unsafe stable code, remove it from the signed feed, freeze, revoke affected credentials/tickets, and offer a clean notarized manual DMG while recovering the bridge.

Stable promotion must verify source commit, build, checksum, bundle/team identity, Apple signature, notarization, staple, Gatekeeper, and test-feed EdDSA signature before exact-byte publication. Retain a checksummed complete Pages artifact per release. Old-client bridge tests, legacy-feed retention, and post-iteration Developer ID reissue remain mandatory.
