use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::eligibility::GateError;

/// Complete effective-policy statement produced outside the lander trust domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyStatement {
    /// Fixed GitHub repository node/database identity.
    pub repository_id: u64,
    /// Protected ref, always `refs/heads/main`.
    pub reference: String,
    /// Release-pinned canonical configuration digest.
    pub configuration_digest: String,
    /// Canonical effective policy digest.
    pub policy_digest: String,
    /// Canonical complete bypass actor digest.
    pub bypass_digest: String,
    /// All applicable source types, including inherited parents.
    pub source_types: Vec<String>,
    /// Complete ordered set of applicable ruleset identifiers.
    pub ruleset_ids: Vec<u64>,
    /// Update timestamp paired with every applicable ruleset identifier.
    pub ruleset_updated_at: Vec<i64>,
    /// True only after every source and page was read.
    pub coverage_complete: bool,
    /// Ruleset history head bound by the attestor.
    pub history_head: String,
    /// Issuance time.
    pub issued_at: i64,
    /// Expiry time.
    pub expires_at: i64,
}

/// Latest-push statement produced by the isolated webhook reconciliation lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushStatement {
    /// Fixed repository identity.
    pub repository_id: u64,
    /// Pull request number.
    pub pr_number: u64,
    /// Exact same-repository head ref.
    pub head_ref: String,
    /// Exact candidate object.
    pub candidate: String,
    /// Account that performed the canonical push.
    pub pusher_id: u64,
    /// Canonical delivery identifier.
    pub delivery_id: String,
    /// Canonical GitHub delivery GUID.
    pub delivery_guid: String,
    /// Delivery time from GitHub.
    pub delivered_at: i64,
    /// Time the isolated receiver durably stored the delivery.
    pub received_at: i64,
    /// Immutable ledger digest.
    pub ledger_digest: String,
    /// Complete delivery-list page chain digest.
    pub page_chain_digest: String,
    /// High-water delivery identifier.
    pub high_water_id: u64,
    /// Time the terminal reconciliation watermark was observed.
    pub watermark_observed_at: i64,
    /// End of the delivery API retention window used by reconciliation.
    pub retention_expires_at: i64,
    /// True only when API reconciliation reached the terminal page.
    pub reconciliation_complete: bool,
    /// Issuance time.
    pub issued_at: i64,
    /// Expiry time.
    pub expires_at: i64,
}

/// Signed policy envelope. The signature is raw Ed25519 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPolicyStatement {
    /// Signed payload.
    pub statement: PolicyStatement,
    /// Detached signature.
    pub signature: Vec<u8>,
}

/// Signed latest-push envelope. The signature is raw Ed25519 bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedPushStatement {
    /// Signed payload.
    pub statement: PushStatement,
    /// Detached signature.
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct PinnedAttestors {
    pub(crate) policy_key: VerifyingKey,
    pub(crate) push_key: VerifyingKey,
    pub(crate) configuration_digest: String,
    pub(crate) policy_digest: String,
    pub(crate) bypass_digest: String,
}

impl PinnedAttestors {
    pub(crate) fn production() -> Result<Self, ()> {
        // Public keys are immutable release configuration. These RFC 8032
        // verification keys contain no signing material and are deliberately
        // distinct for policy and push evidence.
        let policy_key = VerifyingKey::from_bytes(&[
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
            0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
            0xf7, 0x07, 0x51, 0x1a,
        ])
        .map_err(|_| ())?;
        let push_key = VerifyingKey::from_bytes(&[
            0x3d, 0x40, 0x17, 0xc3, 0xe8, 0x43, 0x89, 0x5a, 0x92, 0xb7, 0x0a, 0xa7, 0x4d, 0x1b,
            0x7e, 0xbc, 0x9c, 0x98, 0x2c, 0xcf, 0x2e, 0xc4, 0x96, 0x8c, 0xc0, 0xcd, 0x55, 0xf1,
            0x2a, 0xf4, 0x66, 0x0c,
        ])
        .map_err(|_| ())?;
        Ok(Self {
            policy_key,
            push_key,
            configuration_digest: "tgfs-ruleset-revision-7".into(),
            policy_digest: "BUILD_MUST_REPLACE_POLICY_DIGEST".into(),
            bypass_digest: "BUILD_MUST_REPLACE_BYPASS_DIGEST".into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_new(
        policy_key: VerifyingKey,
        push_key: VerifyingKey,
        configuration_digest: String,
        policy_digest: String,
        bypass_digest: String,
    ) -> Self {
        Self {
            policy_key,
            push_key,
            configuration_digest,
            policy_digest,
            bypass_digest,
        }
    }
}

fn verify<T: Serialize>(key: &VerifyingKey, payload: &T, bytes: &[u8]) -> Result<(), GateError> {
    let encoded = serde_json::to_vec(payload).map_err(|_| GateError::MalformedAttestation)?;
    let signature = Signature::try_from(bytes).map_err(|_| GateError::MalformedAttestation)?;
    key.verify(&encoded, &signature)
        .map_err(|_| GateError::ForgedAttestation)
}

fn fresh(issued_at: i64, expires_at: i64, now: i64) -> bool {
    issued_at <= now
        && now.saturating_sub(issued_at) <= 60
        && expires_at >= now
        && expires_at.saturating_sub(issued_at) <= 120
}

pub(crate) fn verify_policy(
    signed: &SignedPolicyStatement,
    repo: u64,
    now: i64,
    keys: &PinnedAttestors,
) -> Result<(), GateError> {
    verify(&keys.policy_key, &signed.statement, &signed.signature)?;
    let statement = &signed.statement;
    if statement.repository_id != repo
        || statement.reference != "refs/heads/main"
        || statement.configuration_digest != keys.configuration_digest
        || statement.policy_digest != keys.policy_digest
        || statement.bypass_digest != keys.bypass_digest
        || !statement.coverage_complete
        || statement.source_types
            != ["Repository", "Organization", "Enterprise"]
                .map(str::to_owned)
                .to_vec()
        || statement.ruleset_ids.is_empty()
        || statement.ruleset_ids.len() != statement.ruleset_updated_at.len()
        || statement.ruleset_ids.contains(&0)
        || statement.ruleset_updated_at.contains(&0)
        || statement
            .ruleset_ids
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || statement.history_head.is_empty()
        || !fresh(statement.issued_at, statement.expires_at, now)
    {
        return Err(GateError::PolicyAttestation);
    }
    Ok(())
}

pub(crate) fn verify_push(
    signed: &SignedPushStatement,
    repo: u64,
    pr: u64,
    head_ref: &str,
    candidate: &str,
    now: i64,
    keys: &PinnedAttestors,
) -> Result<(), GateError> {
    verify(&keys.push_key, &signed.statement, &signed.signature)?;
    let statement = &signed.statement;
    if statement.repository_id != repo
        || statement.pr_number != pr
        || statement.head_ref != head_ref
        || statement.candidate != candidate
        || statement.pusher_id == 0
        || statement.delivery_id.is_empty()
        || statement.delivery_guid.is_empty()
        || statement.ledger_digest.is_empty()
        || statement.page_chain_digest.is_empty()
        || statement.high_water_id == 0
        || !statement.reconciliation_complete
        || statement.delivered_at > statement.received_at
        || statement.received_at > statement.watermark_observed_at
        || statement.watermark_observed_at > statement.issued_at
        || statement.retention_expires_at < now
        || !fresh(statement.issued_at, statement.expires_at, now)
    {
        return Err(GateError::PushAttestation);
    }
    Ok(())
}
