# GramDrive update operations runbook

This public runbook is the operator contract for Sparkle update delivery. Never put credential values, private-key material, key fingerprints, certificates, runtime Telegram inputs, or decoded files in source, shell history, logs, prompts, board resources, Actions artifacts, releases, or Pages. The repository setter accepts values only on standard input or from owner-only files and suppresses `gh` output; it never puts a value in an argument or prints one.

## One-time bootstrap

1. Create `updates-test`, restricted to protected `main`, with no approval rule. Create `release` with an owner reviewer and no self-review/bypass where GitHub supports it; its deployment-ref policy must permit both protected `main` workflow dispatches and validated `v*` tag promotions. Create `github-pages` with a deployment-ref policy restricted to protected `main`, and enable Pages from GitHub Actions.
2. Provision a dedicated runner account and label. Permit only trusted signing workflows; block untrusted concurrent jobs, interactive access, unrelated processes, and untrusted runner administration while signing runs. Treat workflow and runner-admin changes as signing-key access.
3. Use a local operator machine and a non-synchronised, owner-only staging directory. Its directory mode must be `0700` and each named input file mode `0600`; obtain the bytes out of band and never type them into a terminal. The file contents are, respectively, the base64 Developer ID `.p12`, its export password, and the three replacement notary fields. Do not leave this directory on the runner.
4. Store the five Apple credentials with the repository helper. It validates the permissions before reading each file, streams each value to `gh secret set NAME --env updates-test` on stdin, and prints only public names:

   ```sh
   python3 .scripts/release/check_update_secret_inventory.py --set-developer-id-from "$SECURE_INPUT_DIR"
   python3 .scripts/release/check_update_secret_inventory.py --set-notary-from "$SECURE_INPUT_DIR"
   ```

5. Independently generate test and stable Sparkle keypairs offline. Mount the encrypted offline escrow volume only for this procedure, make its directory owner-only (`0700`), and use a distinct, public, versioned Keychain account name for each channel/generation. `generate_keys` creates that account's keypair; `-x` only exports an existing account and therefore always requires an owner-only private-key-file operand. The following commands create V1, move its sole non-Keychain private export into encrypted escrow, then stream its base64 bytes directly from escrow to the designated environment secret. They have no diagnostic consumer and must not be run with shell tracing:

   ```sh
   export SPARKLE_ESCROW_DIR=/Volumes/GramDriveUpdateEscrow
   export SPARKLE_STAGE_DIR="$SECURE_INPUT_DIR/sparkle"
   mkdir -m 700 "$SPARKLE_STAGE_DIR"
   generate_keys --account GramDrive-Sparkle-Test-V1 >/dev/null
   generate_keys --account GramDrive-Sparkle-Test-V1 -p > "$SPARKLE_STAGE_DIR/test-v1.public"
   generate_keys --account GramDrive-Sparkle-Test-V1 -x "$SPARKLE_STAGE_DIR/test-v1.private"
   mv "$SPARKLE_STAGE_DIR/test-v1.private" "$SPARKLE_ESCROW_DIR/test-v1.private"
   base64 < "$SPARKLE_ESCROW_DIR/test-v1.private" | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64

   generate_keys --account GramDrive-Sparkle-Stable-V1 >/dev/null
   generate_keys --account GramDrive-Sparkle-Stable-V1 -p > "$SPARKLE_STAGE_DIR/stable-v1.public"
   generate_keys --account GramDrive-Sparkle-Stable-V1 -x "$SPARKLE_STAGE_DIR/stable-v1.private"
   mv "$SPARKLE_STAGE_DIR/stable-v1.private" "$SPARKLE_ESCROW_DIR/stable-v1.private"
   base64 < "$SPARKLE_ESCROW_DIR/stable-v1.private" | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64
   ```

   Each `-p` command writes the one-line **public** `SUPublicEDKey` value to the named staging file. Copy that exact public line into the pending, reviewed test or stable channel configuration with its generation-specific feed URL; then remove the public staging records and directory before unmounting escrow:

   ```sh
   rm "$SPARKLE_STAGE_DIR/test-v1.public" "$SPARKLE_STAGE_DIR/stable-v1.public"
   rmdir "$SPARKLE_STAGE_DIR"
   ```

   Commit only that public key/feed URL after review. Run the test and stable sequences in separate offline key-generation sessions; never reuse or copy an export between `updates-test` and `release`. If a staging copy remains after a failed `mv`, destroy it with the organisation's approved local destruction procedure before continuing; do not create an unencrypted backup.

6. Then run the value-free preflight:

   ```sh
   python3 .scripts/release/check_update_secret_inventory.py --check-github
   ```

7. A `release` owner approves stable promotion only after the candidate's exact checksum, commit, build, Apple verification, test-feed verification, and second-Mac evidence are recorded.

After storage, remove the staging files and directory using the organisation's approved local destruction procedure, then record only generation, custodian, authorization, and time for offline escrow. The preflight calls only `gh secret list --env ...`, compares names, and returns non-zero for a missing or unexpected name.

## Stable promotion and Pages deployment

Stable publication is deliberately two-step so the `github-pages` environment remains restricted to protected `main`:

1. Push the reviewed immutable `vMAJOR.MINOR.PATCH` tag, or dispatch `promote`/`rotate-key` from protected `main`, and approve the `release` environment. The promotion must publish and attest the exact candidate DMG plus the complete frozen Pages site on the immutable stable Release. It intentionally does not request a Pages deployment.
2. Verify that the Release contains the complete expected immutable asset inventory and that promotion recorded no byte mismatch. If promotion stopped partway through Release publication, rerun the same tag or main-based promotion operation; immutable comparison makes the rerun resumable. Do not deploy an incomplete Release.
3. From protected `main`, dispatch the approved site deployment for that exact tag:

   ```sh
   gh workflow run release.yml --ref main \
     --field operation=redeploy-site \
     --field tag=vMAJOR.MINOR.PATCH
   ```

4. Approve the `release` environment for the `redeploy-site` job. It downloads only the frozen Release site, verifies the tag-bound GitHub attestations, stable-key manifest signature, archive, and exact inventory, then uploads the authenticated site. The dependent `deploy-pages` job is the only Pages/OIDC holder and reaches branch-scoped `github-pages` from protected `main`.
5. Verify the anonymous versioned stable feed, notes, and enclosure bytes and signatures. If authentication or Pages deployment fails, stop and repair or rerun `redeploy-site`; the previous valid Pages site remains live. Never move the tag, overwrite Release assets, rebuild, re-sign, or downgrade the feed as recovery.

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

1. Generate and escrow V2 in a separate offline session, then set it only in `release` without an argv value. This sequence creates the distinct V2 Keychain identity before exporting it, and removes the staging export after it reaches encrypted escrow:

   ```sh
   export SPARKLE_ESCROW_DIR=/Volumes/GramDriveUpdateEscrow
   export SPARKLE_STAGE_DIR="$SECURE_INPUT_DIR/sparkle"
   mkdir -m 700 "$SPARKLE_STAGE_DIR"
   generate_keys --account GramDrive-Sparkle-Stable-V2 >/dev/null
   generate_keys --account GramDrive-Sparkle-Stable-V2 -p > "$SPARKLE_STAGE_DIR/stable-v2.public"
   generate_keys --account GramDrive-Sparkle-Stable-V2 -x "$SPARKLE_STAGE_DIR/stable-v2.private"
   mv "$SPARKLE_STAGE_DIR/stable-v2.private" "$SPARKLE_ESCROW_DIR/stable-v2.private"
   base64 < "$SPARKLE_ESCROW_DIR/stable-v2.private" | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_STABLE_V2_EDDSA_PRIVATE_KEY_B64
   ```

   The public file is the exact V2 `SUPublicEDKey` input for review. Review and commit it with `https://relux-works.github.io/tgfs/updates/stable/v2/stable.xml`, then remove that public staging file and directory before unmounting escrow:

   ```sh
   rm "$SPARKLE_STAGE_DIR/stable-v2.public"
   rmdir "$SPARKLE_STAGE_DIR"
   ```

   Do not change Developer ID.
2. Build a higher-build stable bridge embedding the V2 key/URL. Publish exact bytes through test and validate appcast signature plus installed bundle configuration.
3. With release approval, use V1 to publish the final `.../stable/v1/stable.xml` whose highest item is the bridge and whose enclosure is V1-signed. In the same complete Pages site publish V2 signed by V2. Never sign V1's URL with V2.
4. Test a previously installed V1 stable client before publication, after it sees the bridge, after relaunch onto V2, and after a later V2-only update. Record only build and feed generation.
5. Cut new downloads/releases to V2 only after that passes. Keep the V1 URL, old-key-signed bridge feed, bridge DMG, checksums, and release bytes live for supported clients. Freeze exact legacy bytes as checksummed immutable release assets.
6. Retire V1 only after the old-key bridge URL is frozen, the immutable checksummed legacy-feed/release bytes are verified, and the V2-only update has passed on the old client. The following contains secret **names only**. It records the active release environment inventory before deletion, removes V1 from active CI, proves V1 absent and V2 present, then deletes the local account-scoped Keychain private-key item. Do not run the Keychain deletion until the V2 secret was stored and encrypted escrow was verified:

   ```sh
   set -e
   export SPARKLE_RETIRE_ENV=release
   export SPARKLE_OLD_SECRET=SPARKLE_STABLE_V1_EDDSA_PRIVATE_KEY_B64
   export SPARKLE_NEW_SECRET=SPARKLE_STABLE_V2_EDDSA_PRIVATE_KEY_B64
   export SPARKLE_OLD_ACCOUNT=GramDrive-Sparkle-Stable-V1
   mkdir -m 700 "$SPARKLE_STAGE_DIR"
   gh secret list --env "$SPARKLE_RETIRE_ENV" --json name > "$SPARKLE_STAGE_DIR/stable-v1-before-retirement.json"
   grep -F "\"$SPARKLE_OLD_SECRET\"" "$SPARKLE_STAGE_DIR/stable-v1-before-retirement.json"
   grep -F "\"$SPARKLE_NEW_SECRET\"" "$SPARKLE_STAGE_DIR/stable-v1-before-retirement.json"
   gh secret delete "$SPARKLE_OLD_SECRET" --env "$SPARKLE_RETIRE_ENV"
   gh secret list --env "$SPARKLE_RETIRE_ENV" --json name > "$SPARKLE_STAGE_DIR/stable-v1-after-retirement.json"
   ! grep -F "\"$SPARKLE_OLD_SECRET\"" "$SPARKLE_STAGE_DIR/stable-v1-after-retirement.json"
   grep -F "\"$SPARKLE_NEW_SECRET\"" "$SPARKLE_STAGE_DIR/stable-v1-after-retirement.json"
   security delete-generic-password -s https://sparkle-project.org -a "$SPARKLE_OLD_ACCOUNT"
   rm "$SPARKLE_STAGE_DIR/stable-v1-before-retirement.json" "$SPARKLE_STAGE_DIR/stable-v1-after-retirement.json"
   rmdir "$SPARKLE_STAGE_DIR"
   ```

   Retain the escrow export through the support/recovery window. Its later destruction is a separate authorization: two custodians record generation, escrow location, time, authorization, and the closed recovery window, then the authorized escrow custodian destroys the encrypted escrow copy under the organisation's approved escrow procedure. Never include the value in that record.

Test-key rotation uses this analogous independent V2 sequence; its account, export, secret, and feed URL must never be substituted with stable values:

```sh
export SPARKLE_ESCROW_DIR=/Volumes/GramDriveUpdateEscrow
export SPARKLE_STAGE_DIR="$SECURE_INPUT_DIR/sparkle"
mkdir -m 700 "$SPARKLE_STAGE_DIR"
generate_keys --account GramDrive-Sparkle-Test-V2 >/dev/null
generate_keys --account GramDrive-Sparkle-Test-V2 -p > "$SPARKLE_STAGE_DIR/test-v2.public"
generate_keys --account GramDrive-Sparkle-Test-V2 -x "$SPARKLE_STAGE_DIR/test-v2.private"
mv "$SPARKLE_STAGE_DIR/test-v2.private" "$SPARKLE_ESCROW_DIR/test-v2.private"
base64 < "$SPARKLE_ESCROW_DIR/test-v2.private" | python3 .scripts/release/check_update_secret_inventory.py --set SPARKLE_TEST_V2_EDDSA_PRIVATE_KEY_B64
```

The public file is the exact V2 `SUPublicEDKey` input for review. Review and commit it with `https://github.com/relux-works/tgfs/releases/download/updates-test-v2/test.xml`, then remove it and the staging directory:

```sh
rm "$SPARKLE_STAGE_DIR/test-v2.public"
rmdir "$SPARKLE_STAGE_DIR"
```

V1 `updates-test-v1/test.xml` signed by V1 offers a bridge embedding V2 and `updates-test-v2/test.xml`; keep V1 live for supported test clients. Retire test V1 only after its old-key bridge URL is frozen and verified, the V2 secret is stored and encrypted escrow is verified, and an old test V1 client has installed the bridge and passed a later V2-only update. The equivalent test V1 retirement is:

```sh
set -e
export SPARKLE_RETIRE_ENV=updates-test
export SPARKLE_OLD_SECRET=SPARKLE_TEST_V1_EDDSA_PRIVATE_KEY_B64
export SPARKLE_NEW_SECRET=SPARKLE_TEST_V2_EDDSA_PRIVATE_KEY_B64
export SPARKLE_OLD_ACCOUNT=GramDrive-Sparkle-Test-V1
mkdir -m 700 "$SPARKLE_STAGE_DIR"
gh secret list --env "$SPARKLE_RETIRE_ENV" --json name > "$SPARKLE_STAGE_DIR/test-v1-before-retirement.json"
grep -F "\"$SPARKLE_OLD_SECRET\"" "$SPARKLE_STAGE_DIR/test-v1-before-retirement.json"
grep -F "\"$SPARKLE_NEW_SECRET\"" "$SPARKLE_STAGE_DIR/test-v1-before-retirement.json"
gh secret delete "$SPARKLE_OLD_SECRET" --env "$SPARKLE_RETIRE_ENV"
gh secret list --env "$SPARKLE_RETIRE_ENV" --json name > "$SPARKLE_STAGE_DIR/test-v1-after-retirement.json"
! grep -F "\"$SPARKLE_OLD_SECRET\"" "$SPARKLE_STAGE_DIR/test-v1-after-retirement.json"
grep -F "\"$SPARKLE_NEW_SECRET\"" "$SPARKLE_STAGE_DIR/test-v1-after-retirement.json"
security delete-generic-password -s https://sparkle-project.org -a "$SPARKLE_OLD_ACCOUNT"
rm "$SPARKLE_STAGE_DIR/test-v1-before-retirement.json" "$SPARKLE_STAGE_DIR/test-v1-after-retirement.json"
rmdir "$SPARKLE_STAGE_DIR"
```

Retain the test V1 escrow export through its documented support/recovery window; its destruction needs the same separate authorization as stable escrow. If every test client is disposable, revocation plus a manual reinstall is permitted only when that discontinuity is recorded.

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
