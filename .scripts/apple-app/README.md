# macOS app packaging

How the GramDrive desktop app reaches users: one signed, notarizable
`GramDrive.app` — the menu-bar companion shell, the launchd background agent, and
the File Provider extension appex — assembled from the `apple/GramDriveSupport`
SwiftPM package over the staged Rust core, and the provenance that makes the
result attributable to a commit without embedding any credential.

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
reviewed v1 GitHub Release feed and its reviewed public EdDSA key.
`--update-channel stable` embeds only the reviewed v1 Pages feed and its
different reviewed public EdDSA key. Both feed/key pairs are checked-in public
configuration, validated before assembly, and never read from environment or
CLI input. `Version.json` is the reviewed three-part marketing version source,
while the numeric `CFBundleVersion` remains the commit count.

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
