# BUG-260720-116eli: release-attestation-plan-gate

## Description
Release run 29703775352 (v0.1.0 at 4ada843) reached the final steps on gramdrive-mac - signing, notarization and stapling all succeeded - then failed at Attest build provenance: GitHub returns Feature not available for the relux-works organization (artifact attestations require a paid plan for private repos, or a public repo). gh release create never ran. Fix release.yml: make both attest steps plan-aware - attempt attestation, and when the API rejects with the feature-unavailable error, degrade gracefully: do not fail the release, record attestation: unavailable (private-repo plan) in the release manifest and job summary so the provenance gap is explicit, keep checksums+SBOM+notarization as the integrity story. If the plan later supports it, attestation resumes with zero config. Then re-point the unpublished v0.1.0 tag to the fix commit and confirm the release run goes green end-to-end through gh release create with the dmg asset.

## Scope
(define bug scope / affected area)

## Acceptance Criteria
Release run on gramdrive-mac completes green end-to-end on this private repo without attestation entitlement; manifest and job summary explicitly record the attestation gap; attestation still hard-fails on real errors when the feature is available; v0.1.0 GitHub Release exists with the dmg asset.
