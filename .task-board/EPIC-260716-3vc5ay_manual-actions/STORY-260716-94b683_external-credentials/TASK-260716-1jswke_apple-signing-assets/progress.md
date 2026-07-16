## Status
done

## Assigned To
(none)

## Created
2026-07-16T10:42:43Z

## Last Update
2026-07-16T21:51:06Z

## Blocked By
- (none)

## Blocks
- TASK-260715-1dk9ik
- TASK-260715-3vd91i
- TASK-260715-1zydwj

## Checklist
- [x] ASC notarization API key stored locally (~/.private_keys/AuthKey_52TSFQH37D.p8) and notarytool keychain profile gramdrive-notary validated
- [x] ASC secrets set on relux-works/tgfs: APPSTORE_ISSUER_ID, APPSTORE_KEY_ID, APPSTORE_P8
- [x] Copy Developer ID p12 to tgfs repo secrets MACOS_CERT_P12 + MACOS_CERT_PASSWORD (same values as barycenter; identity 262RZ595FP present in local keychain)
- [x] Register App Group + File Provider entitlements/identifiers for com.reluxworks.gramdrive.*

## Notes
2026-07-17: notarization path fully working (profile gramdrive-notary validated against Apple). Developer ID Application cert (Relux Works LLC, 262RZ595FP) exists in local keychain and as barycenter repo secrets — remaining human steps: copy p12 secrets to tgfs, register App Group/FP identifiers for com.reluxworks.gramdrive.*.
Identifier plan (2026-07-17): App Group group.com.reluxworks.gramdrive registered manually in the portal (iOS + macOS 15+ future); macOS 14 v1 builds use the team-prefixed group 262RZ595FP.com.reluxworks.gramdrive in entitlements — needs NO portal registration and no provisioning profile with Developer ID. Explicit App IDs com.reluxworks.gramdrive and com.reluxworks.gramdrive.fileprovider with App Groups capability — Xcode automatic signing may auto-register them. File Provider itself needs no Apple-approved entitlement.
2026-07-17: complete. App Group group.com.reluxworks.gramdrive + explicit App IDs com.reluxworks.gramdrive and .fileprovider registered with App Groups capability (owner, portal). MACOS_CERT_P12 (empty password, as in barycenter) + MACOS_CERT_PASSWORD set on relux-works/tgfs from local relux-devid.p12 — verified Developer ID Application 262RZ595FP. All four checklist items done.

## Precondition Resources
(none)

## Outcome Resources
(none)
