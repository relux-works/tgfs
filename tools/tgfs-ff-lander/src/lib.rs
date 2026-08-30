//! Fixed, fail-closed landing core for the sole `relux-works/tgfs` lane.

mod attestation;
mod eligibility;
mod protocol;
mod rollout;

use std::ffi::OsString;

pub use attestation::{PolicyStatement, PushStatement, SignedPolicyStatement, SignedPushStatement};
pub use eligibility::{
    CheckRun, CommitEvidence, CompareState, PageSet, PullRequest, Review, ReviewState, Snapshot,
    StatusContext, Thread,
};
pub use rollout::{
    RolloutSnapshot, Ruleset, RulesetPlan, desired_ruleset, rollback_plan, rollout_plan,
};

use attestation::PinnedAttestors;
use eligibility::Attempt;
use protocol::{Advertisement, ReceiveStatus};

/// Process exit used for invalid production argv.
pub const EX_USAGE: i32 = 64;
/// Process exit used when a security or protocol gate refuses the landing.
pub const EX_REFUSED: i32 = 77;
const FIXED_REPOSITORY_ID: u64 = 81_611_324;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrNumber(u64);

fn parse_args<I>(args: I) -> Result<PrNumber, ()>
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    if args.len() != 4 || args[1] != "land" || args[2] != "--pr" {
        return Err(());
    }
    let raw = args[3].to_str().ok_or(())?;
    if raw.is_empty() || raw.starts_with('0') || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    let number = raw.parse::<u64>().map_err(|_| ())?;
    if number == 0 {
        return Err(());
    }
    Ok(PrNumber(number))
}

trait FixedBackend {
    fn now_unix(&self) -> i64;
    fn read_attempt(&mut self, pr: u64) -> Result<Attempt, BackendError>;
    fn audit_ready(&mut self) -> Result<(), BackendError>;
    fn mint_short_lived_token(&mut self) -> Result<SecretToken, BackendError>;
    fn advertise_receive_pack(&mut self, token: &SecretToken) -> Result<Vec<u8>, BackendError>;
    fn send_receive_pack(
        &mut self,
        token: &SecretToken,
        request: &[u8],
    ) -> Result<Vec<u8>, BackendError>;
    fn read_main(&mut self) -> Result<String, BackendError>;
    fn append_audit(&mut self, record: AuditRecord) -> Result<(), BackendError>;
}

#[derive(Debug)]
struct SecretToken(Vec<u8>);

impl Drop for SecretToken {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug)]
struct BackendError;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuditRecord {
    pr: u64,
    old_id: String,
    new_id: String,
}

struct SealedProductionBackend;

impl FixedBackend for SealedProductionBackend {
    fn now_unix(&self) -> i64 {
        0
    }

    fn read_attempt(&mut self, _pr: u64) -> Result<Attempt, BackendError> {
        Err(BackendError)
    }

    fn audit_ready(&mut self) -> Result<(), BackendError> {
        Err(BackendError)
    }

    fn mint_short_lived_token(&mut self) -> Result<SecretToken, BackendError> {
        Err(BackendError)
    }

    fn advertise_receive_pack(&mut self, _token: &SecretToken) -> Result<Vec<u8>, BackendError> {
        Err(BackendError)
    }

    fn send_receive_pack(
        &mut self,
        _token: &SecretToken,
        _request: &[u8],
    ) -> Result<Vec<u8>, BackendError> {
        Err(BackendError)
    }

    fn read_main(&mut self) -> Result<String, BackendError> {
        Err(BackendError)
    }

    fn append_audit(&mut self, _record: AuditRecord) -> Result<(), BackendError> {
        Err(BackendError)
    }
}

fn run<B: FixedBackend>(args: Vec<OsString>, backend: &mut B, keys: &PinnedAttestors) -> i32 {
    let pr = match parse_args(args) {
        Ok(pr) => pr,
        Err(()) => return EX_USAGE,
    };
    let attempt = match backend.read_attempt(pr.0) {
        Ok(attempt) => attempt,
        Err(_) => return EX_REFUSED,
    };
    if eligibility::verify_attempt(
        &attempt,
        pr.0,
        FIXED_REPOSITORY_ID,
        backend.now_unix(),
        keys,
    )
    .is_err()
        || backend.audit_ready().is_err()
    {
        return EX_REFUSED;
    }

    let expected_old = attempt.final_snapshot.main.clone();
    let candidate = attempt.final_snapshot.pull.candidate.clone();
    let token = match backend.mint_short_lived_token() {
        Ok(token) => token,
        Err(_) => return EX_REFUSED,
    };
    let advertisement = match backend.advertise_receive_pack(&token) {
        Ok(bytes) => match Advertisement::parse(&bytes) {
            Ok(advertisement) => advertisement,
            Err(_) => return EX_REFUSED,
        },
        Err(_) => return EX_REFUSED,
    };
    if advertisement.main != expected_old {
        return EX_REFUSED;
    }
    let request = match protocol::fixed_update(&expected_old, &candidate) {
        Ok(request) => request,
        Err(_) => return EX_REFUSED,
    };
    let response = match backend.send_receive_pack(&token, &request) {
        Ok(response) => response,
        Err(_) => return EX_REFUSED,
    };
    if ReceiveStatus::parse(&response).is_err() {
        return EX_REFUSED;
    }
    let post_main = match backend.read_main() {
        Ok(post_main) => post_main,
        Err(_) => return EX_REFUSED,
    };
    if post_main != candidate {
        return EX_REFUSED;
    }
    if backend
        .append_audit(AuditRecord {
            pr: pr.0,
            old_id: expected_old,
            new_id: candidate,
        })
        .is_err()
    {
        return EX_REFUSED;
    }
    0
}

/// Exec-only production call site. The deployable image replaces the sealed
/// backend with repository-fixed wiring; invalid argv is rejected before any
/// capability can be initialized.
pub fn production_main<I>(args: I) -> i32
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    if parse_args(args.clone()).is_err() {
        return EX_USAGE;
    }
    let keys = match PinnedAttestors::production() {
        Ok(keys) => keys,
        Err(()) => return EX_REFUSED,
    };
    // This source build is intentionally disabled until release engineering
    // replaces the sealed backend with the reviewed fixed deployment wiring.
    // Crucially, canonical argv still enters the exact production pipeline;
    // the disabled backend fails on its first read and cannot mint a token.
    run(args, &mut SealedProductionBackend, &keys)
}

#[cfg(test)]
mod tests;
