# BUG-260720-29dn2v: release-missing-devid-ca

## Description
First live release run on the self-hosted runner (29702885260, tag v0.1.0 at 99ad6a9) failed at the keychain-import step: identity imports (1 identity imported) but security find-identity -v -p codesigning reports 0 valid Developer ID Application identities, so the grep -c check exits 1. Root cause confirmed: relux lacks the Apple Developer ID Certification Authority intermediate certificate (CA-MISSING in System keychain) which GitHub-hosted runners preinstall; without the chain the imported identity is not valid for codesigning. Fix in release.yml (self-sufficient on any fresh runner): download Apple DeveloperIDG2CA.cer pinned by sha256 (source https://www.apple.com/certificateauthority/), import it into the same throwaway keychain before the find-identity check; keep the check. Then re-point the unpublished v0.1.0 tag to the fix commit (delete+retag+push, prior runs produced no release) and watch the release run on gramdrive-mac to a green conclusion incl. notarization and gh release create.

## Scope
(define bug scope / affected area)

## Acceptance Criteria
release.yml imports the pinned Developer ID CA into the throwaway keychain and the find-identity check reports >=1 valid identity on the self-hosted runner; the v0.1.0 release run completes green end-to-end (sign, notarize, staple, gh release create) on gramdrive-mac; no residual certs/secrets outside the run lifetime beyond the CA itself.
