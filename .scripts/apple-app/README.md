# macOS app packaging

How the GramDrive desktop app reaches users: one signed, notarizable
`GramDrive.app` — the menu-bar companion shell, the launchd background agent, and
the File Provider extension appex — assembled from the `apple/GramDriveSupport`
SwiftPM package over the staged Rust core, and the provenance that makes the
result attributable to a commit without embedding any credential.

`build_qa_fault_bundle.py` is the explicit non-shipping wrapper used only for
BUG-260729-3uclm3 installed File Provider fault acceptance. It requires a
private mode-0600 per-build secret and enables a compile-time Swift target that
ordinary `build_app_bundle.py` actively scrubs and rejects at the binary-byte
boundary. QA builds must not be notarized, uploaded, or installed over a real
user profile; see `.scripts/acceptance/README.md` for the dedicated-profile
procedure.

Owned by TASK-260715-1dk9ik (STORY-260715-2ca0k9, EPIC-260715-3i9uyp). The
tag-triggered GitHub `release.yml` that *invokes* this script is a separate task
(TASK-260715-3bhbkv); this directory owns the pipeline, not the CI wiring.

```sh
GRAMDRIVE_TDLIB_ARTIFACT_DIR=.temp/tdlib/out make package
                             # stage the live tdjson-linked core first
# The packager defaults to test; use --update-channel stable for a stable candidate.
make package-app             # build + sign + verify the app and dmg (no notarization)
make package-app-notarize    # the full path: also notarize + staple via gramdrive-notary
```

Output lands in `.temp/app-packaging/` (gitignored). Artifacts are built, never
committed: a checked-in signed binary is a binary nobody can attribute to a
commit, and its signature would be stale the moment the toolchain moved.

## What it produces

```
.temp/app-packaging/
  GramDrive.app/                     the signed, hardened-runtime bundle
    Contents/
      Info.plist                     com.reluxworks.gramdrive
      Frameworks/Sparkle.framework   Sparkle 2.9.5 plus Autoupdate, Updater.app,
                                     Downloader.xpc, and Installer.xpc
      Resources/ThirdPartyLicenses/OpenSSL.txt
                                     license for OpenSSL bytes inside libtdjson
      PkgInfo                        APPL????
      MacOS/GramDrive                the menu-bar companion shell
      MacOS/gramdrive-agent          the launchd-run engine-hosting agent
      Library/LaunchAgents/com.reluxworks.gramdrive.agent.plist
      PlugIns/GramDriveFileProvider.appex/Contents/
        Info.plist                   com.reluxworks.gramdrive.fileprovider + NSExtension
        MacOS/GramDriveFileProvider  the NSFileProviderReplicatedExtension host
  GramDrive-<version>.dmg            signed (+ stapled when notarized)
  entitlements/                      the generated entitlements, for review + provenance
  manifest.json                      identity, per-binary entitlements + cdhash, checksums
  CHECKSUMS.sha256                   sha256 of the dmg and every app-bundle file
  logs/                              per-step combined output
```

## The three properties this pipeline exists to guarantee

## Sparkle channels

The packager embeds one trust channel at build time; there is no runtime
test/stable selector. `--update-channel test` (the default) embeds only the
reviewed v1 GitHub Release feed and its reviewed public EdDSA key. Test clients
are disposable: `updates-test-v1` is frozen, rather than bridged into stable
trust, if its key is ever retired. `--update-channel stable` defaults to the
reviewed v1 Pages feed and its different reviewed public EdDSA key. An approved
rotation candidate uses the generation and public key from the reviewed
`.github/sparkle-stable.json`; the candidate workflow passes that binding to
`--update-feed-generation N --update-public-key KEY`. The packager constructs
the versioned URL itself, rejects v1 key overrides, and records the
generation/key in the signed candidate manifest. `Version.json` is the reviewed
three-part marketing version source. Ordinary packaging uses the positive git
commit count as `CFBundleVersion`. Candidate CI may pass a reviewed
`--build-number` at or above that git floor after inspecting the public feeds;
the manifest records both values and the selection source.

Publication never rebuilds those bytes. The candidate workflow first uploads
one attested exact-byte package. Its downstream `publish-test` job revalidates
that package, signs with only the test EdDSA key, uploads immutable build-named
assets to `updates-test-v1`, and replaces `test.xml` last with restoration of
the prior feed on failure. The tag workflow accepts only a tested
`stable-candidate` package for the exact tag commit/version, compares the
prerelease DMG byte-for-byte, and—after `release` environment approval—signs
with only the stable key. The macOS promotion job publishes the frozen site
archive, signed inventory, and GitHub attestation to the immutable stable
Release, then stops. A keyless GitHub-hosted Ubuntu job downloads and verifies
those exact Release assets before creating the Pages artifact, so the signing
runner needs neither Pages permission nor GNU tar. A final keyless job alone
has `pages: write` and deploys the authenticated complete site. Test and stable
reruns capture each Release's complete structured asset inventory before
upload; malformed, duplicate, or unavailable inventories fail closed, existing
assets are downloaded and byte-compared, and only an explicit absent state can
reach `gh release upload`.

If Pages deployment fails—or a previously accepted site must be restored after
its candidate has aged out of the rolling feed—dispatch `stable-release` with
`operation=redeploy-site` and the exact stable tag. After the same `release`
approval, a keyless read-only job verifies GitHub provenance for the archive,
inventory manifest, and manifest signature, safely materializes the archive,
then verifies the exact inventory and every retained feed/notes signature. The
Pages-only job deploys only those authenticated exact files.

Each stable Release freezes `stable-pages-site.tar.gz`. The next promotion must
restore it successfully before changing the active versioned feed, so frozen
old-key feed generations and their bridge items remain byte-identical while a
new generation is added. Restoration deliberately excludes the current source
tag: an absent, partially uploaded, or complete current Release is regenerated
and immutable-compared against the latest prior semver site's singular assets.
An incomplete latest prior Release fails closed, as does rerunning an older tag
after a newer stable semver has been published. Rotation first lands a reviewed higher generation/key
in `.github/sparkle-stable.json` and builds the tested candidate from that exact
binding. A tag publication then derives the sole prior generation/key from the
authenticated site manifest and automatically performs the one-time bridge;
an explicit `operation=rotate-key` enforces the same transition. Promotion
cannot advance a generation, and repeated rotation cannot rewrite a frozen
feed. After approval the job publishes the same tested higher-build DMG as the
final old-key bridge and first new-key item, verifies both signatures, and
freezes both in one complete site. Later tags read the checked-in active binding
and mutate only that generation. Never change the signer
behind an existing versioned URL. Rollback uses the same candidate/test/stable
path with a new protected-main commit and a greater `CFBundleVersion`; candidate
selection advances above the public feed even when the same commit is built for
both trust channels. Stable-candidate selection separately treats the latest
published semver Release across the complete paginated GitHub response as its
fail-closed state head; it never skips an incomplete head. Malformed pages or
publication timestamps abort before signing or mutation. It authenticates that
Release's attested stable-site manifest, requires every generation recorded
there, and verifies each fetched Pages feed against its recorded SHA-256 and
byte count before using its build floor. An unrecorded generation must remain
404 until its first authenticated publication. This prevents a replaced or
rolled-back Pages response from lowering the historical floor and permits the
reviewed next generation to be absent only until that manifest first records it.
The candidate/stable workflows serialize the final verified handoff against
publication; a lower or concurrently consumed build is never uploaded or
published as a downgrade.

#### Forward-only stable rollback

A released DMG cannot be reused byte-for-byte as a rollback item: its embedded
`CFBundleVersion`, Sparkle metadata, and code signature would still identify the
old build. The executable rollback contract is therefore a new protected-main
commit that restores the known-good source/configuration, advances
`Version.json` to a semver greater than the latest stable tag, and is built and
signed as new bytes with a Sparkle build greater than every authenticated test
and stable feed item.

The operator evidence sequence is:

1. Land the reviewed revert/fix and version bump through protected main. Record
   the exact commit and the prior known-good commit it restores.
2. Let the ordinary push build publish its higher test item and anonymously
   verify the Release DMG digest, `test.xml` build/version, notes, and test-key
   signatures.
3. Dispatch `candidate-build` from `main` with `mode=stable-candidate` and the
   exact protected commit. Verify its attestation, embedded stable endpoint/key,
   immutable DMG name, and strictly higher selected build.
4. Create the new higher semver tag on that same commit and pass the `release`
   approval. Stable promotion must select the tested candidate byte-for-byte,
   publish immutable tag assets, and deploy the authenticated complete site.
5. Anonymously verify the versioned Pages feed/notes, stable-key signatures,
   Release DMG digest, retained old-generation bytes, and successful update from
   the faulty build. Preserve run IDs and hashes as the rollback outcome.

Any failure before the verified Pages artifact leaves the prior site live. If
Release publication succeeds but Pages deployment fails, `redeploy-site` for
the new tag reuses only its authenticated frozen site. Tags, Release assets,
and existing feed items are never overwritten or moved backward.

### Signed and verifiable

Every Mach-O is Developer ID signed with the hardened runtime (`--options
runtime`) and a trusted timestamp (`--timestamp`), and each carries only its own
entitlements. Signing is **inside-out** — the appex and the agent, then the app
that seals them — because codesign refuses to seal a bundle whose nested code is
unsigned or was signed afterward. The result is checked three ways, none of them
assumed:

- `codesign --verify --deep --strict` — the signatures are structurally valid;
- the entitlements of each binary are **dumped and parsed**, then asserted
  against what was meant to be applied (and asserted to lack `get-task-allow`);
- Gatekeeper (`spctl --assess --type exec`) — recorded, not gated: an
  un-notarized Developer ID app is legitimately *rejected* here, and only the
  notarized+stapled artifact turns it to *accepted*, which the notarize run
  re-checks.

The notarized path also re-runs strict verification on the final stapled app
and DMG, runs `stapler validate` on both, and requires Gatekeeper acceptance for
both the executable app and the disk image's primary signature. These final
results are recorded in `signature_verification`, `staple_verification`, and the
per-target `gatekeeper` fields of `manifest.json`.

After all frameworks and runtime libraries are embedded, the same path discovers
every non-symlink Mach-O from its magic bytes and reads back its architectures,
strict signature, `TeamIdentifier`, and Developer ID leaf authority. The
privacy-safe relative-path inventory is recorded in
`shipped_code_verification`; candidate production refuses an incomplete or
mismatched inventory.

Measured on the current build: `codesign --verify --deep --strict` passes,
`flags=0x10000(runtime)` on all three binaries, `TeamIdentifier=262RZ595FP`.

When notarizing, **both the `.app` and the `.dmg` are stapled**, and the `.app`
is stapled *before* the dmg is built — so the copy a user drags out of the
mounted dmg carries its own offline ticket, not just the dmg. (Stapling only the
dmg leaves the extracted app unverifiable offline; the app is zipped and
notarized first so its cdhash is registered, then stapled, then packed.)

### Credential-free

No signing key, notarization key, or Telegram secret is read from or written to
the repository tree (SEC-001, NFR-053). The signing identity is resolved from a
keychain that already holds it — default `Developer ID Application: Relux Works,
LLC (262RZ595FP)`, overridable with `--identity` or `GRAMDRIVE_SIGN_IDENTITY`.
Notarization uses the `gramdrive-notary` notarytool keychain profile (which holds
the ASC API key out of band, TASK-260716-1jswke), or ASC API-key env in CI. The
manifest records the identity's **name and team**, and notarization by
**submission id and status** — never any key material. The self-tests assert the
manifest carries none of the words a leak would (`PRIVATE KEY`, `p12`,
`password`, `AuthKey`, …).

### Attributable

`manifest.json` records the commit, toolchain (Xcode/Swift/rustc), the staged
core's contract version, each binary's bundle id + entitlements + cdhash, and —
when notarized — the submission id and status. `CHECKSUMS.sha256` covers the dmg
and every file in the bundle. For a live TDLib build it also records the
privacy-safe OpenSSL version and exact license digest propagated from the
authoritative TDLib artifact; the license ships inside the signed app and the
candidate gate requires identical bytes across TDLib, core, and app inventories.
The TDLib signing transition separately binds the authoritative/core digest to
the embedded dylib immediately before `codesign`, then binds the final changed
Mach-O digest to `CHECKSUMS.sha256` and the strict nested Developer ID identity
readback. It deliberately does not require pre-sign and post-sign bytes to be
equal.

The signed bytes are **deliberately not byte-reproducible**: Developer ID signing
embeds a trusted timestamp that varies per signature *by design* — that is the
whole point of a trusted timestamp. NFR-052 asks that a release artifact be
reproducibly *attributable* to a commit, which the manifest and checksums
provide; it does not ask a signature to be byte-identical, which it cannot be.
The manifest states this rather than claiming a property it does not have
(`reproducible.byte_identical: false`, `attributable: true`).

## Identifiers and entitlements

Sourced from `.spec/platform-requirements.md` and TASK-260716-1jswke, never
invented here (macOS 14+ arm64, POL-5/DEC-017):

| Binary | Bundle id | Entitlements |
|---|---|---|
| App (menu-bar shell) | `com.reluxworks.gramdrive` | app-groups; hardened runtime; unsandboxed |
| Agent (launchd) | `com.reluxworks.gramdrive.agent` | app-groups; hardened runtime; unsandboxed |
| File Provider (appex) | `com.reluxworks.gramdrive.fileprovider` | app-sandbox **+** app-groups; hardened runtime |

- **App Group `262RZ595FP.com.reluxworks.gramdrive`** — the team-ID-prefixed
  form is the one v1 ships: under Developer ID it needs no portal registration
  and no provisioning profile. `group.com.reluxworks.gramdrive` is the iOS /
  macOS 15+ form and is deliberately not used.
- **No `get-task-allow`.** SwiftPM's debug build stamps every executable with a
  `com.apple.security.get-task-allow=true` entitlement, which fails
  notarization. The pipeline re-signs with its own release entitlements, and the
  verify step asserts the leaked entitlement is gone.
- **App Sandbox only on the extension.** macOS File Provider extensions run in
  the App Sandbox; the App Group is exactly what lets a sandboxed extension reach
  durable state and the agent's hydration socket. The unsandboxed Developer ID
  shell and agent need no sandbox to register domains, run a launchd item, or
  host TDLib.

## The appex entry point (why there is an executable target)

SwiftPM cannot emit an `.appex`, so the package builds
`GramDriveFileProviderExtensionApp` as an executable target and the packaging
script wraps its binary in the extension bundle. `Package.swift` passes
`-e _NSExtensionMain` to the linker, making Foundation's extension runtime the
Mach-O entry point just as it is for an Xcode App Extension product.

`apple/GramDriveSupport/Sources/GramDriveFileProviderExtensionApp/main.swift`
must not call `NSExtensionMain`: doing so would recursively re-enter the
extension runtime before File Provider can deliver a callback. Its only runtime
role is to touch the principal class's metatype so the linker retains the class
that the system later resolves by name. The packaging script supplies the
`.appex` wrapper and Info.plist whose `NSExtension` dictionary names that class
as `GramDriveFileProvider.GramDriveFileProviderExtension`, its Swift-mangled
Objective-C runtime name. Verified with the binary inspection gates: the Mach-O
imports `_NSExtensionMain`, uses it as `LC_MAIN`, and retains
`_OBJC_CLASS_$__TtC21GramDriveFileProvider30GramDriveFileProviderExtension`.

## Requirements

macOS with Xcode (`swift`, `codesign`, `spctl`, `hdiutil`, `xcrun
notarytool`/`stapler`), a keychain holding the Developer ID Application identity,
and the staged core package (`make package`, resolved by default at
`.temp/packaging/GramDriveCore`; override with `--core-package` or
`GRAMDRIVE_CORE_PACKAGE`). POL-5 makes the Apple host the only v1 target; on any
other platform the script exits 2 with that reason rather than a partial
artifact.

## Not in scope here (recorded so it is not silently forgotten)

- **TDLib in the agent.** The live core links the staged arm64
  `libtdjson.dylib`, which is embedded at `Contents/Frameworks` and loaded via
  `@rpath`. OpenSSL is statically linked into that authoritative TDLib artifact;
  the packager rejects libssl/libcrypto load commands and compiled Homebrew,
  user-home, or temporary builder paths. It preserves the exact dylib bytes
  through core staging and pre-sign app assembly, then records the final
  Developer ID-signed bytes separately. The source-built OpenSSL
  `/etc/ssl/cert.pem` trust-store proof remains identical from the authoritative
  TDLib artifact through the app manifest.
- **The release workflow** now exists (`.github/workflows/release.yml`,
  TASK-260715-3bhbkv): tag-triggering, importing the cert from `MACOS_CERT_P12`
  into a throwaway keychain, GitHub artifact attestation, and — via
  `.scripts/release/build_release_provenance.py` — the SBOM, changelog and
  rollback metadata. This script is what that workflow runs to build the signed
  artifact; it is invoked with `--notarize --notary-keychain <the throwaway
  keychain>` so notarization needs no key on disk.

## Self-tests

`.scripts/tests/test_build_app_bundle.py`, run by the `repo` gate suite. They
fake every subprocess, so they cover what the real pipeline cannot be asked to
stage on demand — the exact entitlements, the inside-out signing order, a leaked
`get-task-allow`, a notarization rejection, a missing core package — and they run
on a machine without Xcode, a signing identity, or network.
