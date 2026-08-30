use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::attestation::{
    PinnedAttestors, SignedPolicyStatement, SignedPushStatement, verify_policy, verify_push,
};

/// A fully consumed paginated result, with the observed `next` cursor per page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSet<T> {
    /// Result pages in read order.
    pub pages: Vec<Vec<T>>,
    /// `next` cursor for each page; the last must be absent.
    pub next: Vec<Option<String>>,
}

impl<T> PageSet<T> {
    fn exhausted(&self) -> bool {
        if self.pages.is_empty()
            || self.pages.len() != self.next.len()
            || self.next.last() != Some(&None)
        {
            return false;
        }
        let mut cursors = HashSet::new();
        self.next
            .iter()
            .take(self.next.len().saturating_sub(1))
            .all(|cursor| {
                cursor
                    .as_ref()
                    .is_some_and(|value| !value.is_empty() && cursors.insert(value))
            })
    }

    fn values(&self) -> impl Iterator<Item = &T> {
        self.pages.iter().flatten()
    }
}

/// Pull request fields used by the fixed lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// PR state.
    pub open: bool,
    /// Draft marker.
    pub draft: bool,
    /// Base repository identity.
    pub base_repository_id: u64,
    /// Head repository identity; forks are rejected.
    pub head_repository_id: u64,
    /// Base branch.
    pub base_ref: String,
    /// Head branch.
    pub head_ref: String,
    /// Exact head object.
    pub candidate: String,
    /// PR author account.
    pub author_id: u64,
}

/// Compare result between observed main and candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareState {
    /// Candidate is a strict descendant of main.
    Ahead,
    /// Candidate equals main.
    Identical,
    /// Candidate is behind main.
    Behind,
    /// Histories diverge.
    Diverged,
}

/// Commit signature evidence for an introduced object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitEvidence {
    /// Introduced object id.
    pub sha: String,
    /// GitHub verification bit.
    pub github_verified: bool,
    /// GitHub verification reason.
    pub github_reason: String,
    /// Whether the commit is human-authored.
    pub human_authored: bool,
    /// Raw object contains an SSH signature header.
    pub ssh_gpgsig: bool,
    /// `git verify-commit --raw` succeeded in the disposable verifier.
    pub ssh_verified: bool,
    /// Allowed-signers principal matched declared author policy.
    pub principal_matched: bool,
}

/// Pull review state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
    /// Approval.
    Approved,
    /// Requested changes.
    ChangesRequested,
    /// Dismissed review.
    Dismissed,
    /// Non-authorizing comment.
    Commented,
}

/// One chronological review row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// Stable review id.
    pub id: u64,
    /// Reviewer account id.
    pub reviewer_id: u64,
    /// Reviewed commit.
    pub commit_id: String,
    /// State.
    pub state: ReviewState,
    /// Submission timestamp.
    pub submitted_at: i64,
    /// Current repository permission observed independently.
    pub permission: String,
}

/// Review thread resolution row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    /// Stable GraphQL id.
    pub id: String,
    /// Resolution bit.
    pub resolved: bool,
}

/// Check-run evidence on the candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    /// Stable check-run id.
    pub id: u64,
    /// Required context.
    pub name: String,
    /// Producer GitHub App id.
    pub app_id: u64,
    /// GitHub status.
    pub status: String,
    /// GitHub conclusion.
    pub conclusion: String,
    /// Exact checked object.
    pub head_sha: String,
    /// Completion time; must follow the canonical push delivery.
    pub completed_at: i64,
}

/// Legacy commit status row; required names are forbidden here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusContext {
    /// Stable status id.
    pub id: u64,
    /// Status context.
    pub context: String,
}

/// One complete read/revalidation snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Visible active-rules pagination was exhausted.
    pub visible_rules_complete: bool,
    /// Every visible applicable rule was active.
    pub visible_rules_active: bool,
    /// Current main object.
    pub main: String,
    /// Re-read pull request.
    pub pull: PullRequest,
    /// Main/candidate compare result.
    pub compare: CompareState,
    /// Both old and new names of every changed path.
    pub changed_paths: PageSet<String>,
    /// Every introduced commit.
    pub commits: PageSet<CommitEvidence>,
    /// Chronological reviews.
    pub reviews: PageSet<Review>,
    /// GraphQL review threads.
    pub threads: PageSet<Thread>,
    /// All check runs on candidate.
    pub checks: PageSet<CheckRun>,
    /// All legacy statuses on candidate.
    pub statuses: PageSet<StatusContext>,
}

#[derive(Debug, Clone)]
pub(crate) struct Attempt {
    pub(crate) initial_snapshot: Snapshot,
    pub(crate) final_snapshot: Snapshot,
    pub(crate) initial_policy: SignedPolicyStatement,
    pub(crate) final_policy: SignedPolicyStatement,
    pub(crate) initial_push: SignedPushStatement,
    pub(crate) final_push: SignedPushStatement,
}

/// Closed refusal reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateError {
    /// A read did not prove pagination exhaustion.
    PartialRead,
    /// PR identity/state failed.
    PullRequest,
    /// Candidate was not strictly ahead.
    Ancestry,
    /// Workflow path is outside this lane.
    WorkflowPath,
    /// Commit signature failed.
    Signature,
    /// Review authorization failed.
    Review,
    /// A thread is unresolved.
    Thread,
    /// Required source-bound checks failed.
    Check,
    /// Legacy status substitution was present.
    StatusSubstitution,
    /// Attestation encoding/signature was malformed.
    MalformedAttestation,
    /// Signature did not verify with the pinned independent key.
    ForgedAttestation,
    /// Effective-policy evidence failed.
    PolicyAttestation,
    /// Latest-push evidence failed.
    PushAttestation,
    /// Initial and final observations differed.
    RevalidationRace,
}

fn oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn workflow_path(path: &str) -> bool {
    path == ".github/workflows" || path.starts_with(".github/workflows/")
}

fn validate_pages(snapshot: &Snapshot) -> Result<(), GateError> {
    if !snapshot.changed_paths.exhausted()
        || !snapshot.commits.exhausted()
        || !snapshot.reviews.exhausted()
        || !snapshot.threads.exhausted()
        || !snapshot.checks.exhausted()
        || !snapshot.statuses.exhausted()
    {
        return Err(GateError::PartialRead);
    }
    Ok(())
}

fn validate_snapshot(
    snapshot: &Snapshot,
    pr: u64,
    repo: u64,
    push: &SignedPushStatement,
) -> Result<(), GateError> {
    validate_pages(snapshot)?;
    let pull = &snapshot.pull;
    if !pull.open
        || pull.draft
        || pull.base_repository_id != repo
        || pull.head_repository_id != repo
        || pull.base_ref != "main"
        || pull.author_id == 0
        || !oid(&snapshot.main)
        || !oid(&pull.candidate)
        || !snapshot.visible_rules_complete
        || !snapshot.visible_rules_active
    {
        return Err(GateError::PullRequest);
    }
    if snapshot.compare != CompareState::Ahead {
        return Err(GateError::Ancestry);
    }
    if snapshot
        .changed_paths
        .values()
        .any(|path| workflow_path(path))
    {
        return Err(GateError::WorkflowPath);
    }
    let mut commit_ids = HashSet::new();
    if snapshot.commits.values().next().is_none()
        || snapshot.commits.values().any(|commit| {
            !commit_ids.insert(commit.sha.as_str())
                || !oid(&commit.sha)
                || !commit.github_verified
                || commit.github_reason != "valid"
                || (commit.human_authored
                    && (!commit.ssh_gpgsig || !commit.ssh_verified || !commit.principal_matched))
        })
    {
        return Err(GateError::Signature);
    }
    if snapshot
        .threads
        .values()
        .any(|thread| thread.id.is_empty() || !thread.resolved)
    {
        return Err(GateError::Thread);
    }
    let pusher = push.statement.pusher_id;
    let delivered_at = push.statement.delivered_at;
    let mut review_ids = HashSet::new();
    let mut latest: HashMap<u64, &Review> = HashMap::new();
    for review in snapshot.reviews.values() {
        if review.id == 0 || !review_ids.insert(review.id) {
            return Err(GateError::Review);
        }
        if latest
            .get(&review.reviewer_id)
            .is_some_and(|prior| review.submitted_at == prior.submitted_at)
        {
            return Err(GateError::Review);
        }
        let replace = latest
            .get(&review.reviewer_id)
            .is_none_or(|prior| review.submitted_at > prior.submitted_at);
        if replace {
            latest.insert(review.reviewer_id, review);
        }
    }
    let approved = latest.values().any(|review| {
        review.state == ReviewState::Approved
            && review.commit_id == pull.candidate
            && review.submitted_at > delivered_at
            && review.reviewer_id != pull.author_id
            && review.reviewer_id != pusher
            && matches!(review.permission.as_str(), "write" | "maintain" | "admin")
    });
    if !approved {
        return Err(GateError::Review);
    }
    const REQUIRED: [&str; 2] = ["rust-core", "secret-scan"];
    let mut check_ids = HashSet::new();
    if snapshot
        .checks
        .values()
        .any(|check| check.id == 0 || !check_ids.insert(check.id))
    {
        return Err(GateError::Check);
    }
    for required in REQUIRED {
        let matching: Vec<&CheckRun> = snapshot
            .checks
            .values()
            .filter(|check| check.name == required)
            .collect();
        if matching.len() != 1 {
            return Err(GateError::Check);
        }
        let check = matching[0];
        if check.app_id != 15_368
            || check.status != "completed"
            || check.conclusion != "success"
            || check.head_sha != pull.candidate
            || check.completed_at <= delivered_at
        {
            return Err(GateError::Check);
        }
    }
    let mut status_ids = HashSet::new();
    if snapshot.statuses.values().any(|status| {
        status.id == 0
            || !status_ids.insert(status.id)
            || REQUIRED.contains(&status.context.as_str())
    }) {
        return Err(GateError::StatusSubstitution);
    }
    let _ = pr;
    Ok(())
}

pub(crate) fn verify_attempt(
    attempt: &Attempt,
    pr: u64,
    repo: u64,
    now: i64,
    keys: &PinnedAttestors,
) -> Result<(), GateError> {
    for policy in [&attempt.initial_policy, &attempt.final_policy] {
        verify_policy(policy, repo, now, keys)?;
    }
    for (snapshot, push) in [
        (&attempt.initial_snapshot, &attempt.initial_push),
        (&attempt.final_snapshot, &attempt.final_push),
    ] {
        verify_push(
            push,
            repo,
            pr,
            &snapshot.pull.head_ref,
            &snapshot.pull.candidate,
            now,
            keys,
        )?;
        validate_snapshot(snapshot, pr, repo, push)?;
    }
    let initial_policy = &attempt.initial_policy.statement;
    let final_policy = &attempt.final_policy.statement;
    let policy_binding_changed = initial_policy.repository_id != final_policy.repository_id
        || initial_policy.reference != final_policy.reference
        || initial_policy.configuration_digest != final_policy.configuration_digest
        || initial_policy.policy_digest != final_policy.policy_digest
        || initial_policy.bypass_digest != final_policy.bypass_digest
        || initial_policy.source_types != final_policy.source_types
        || initial_policy.ruleset_ids != final_policy.ruleset_ids
        || initial_policy.ruleset_updated_at != final_policy.ruleset_updated_at
        || initial_policy.history_head != final_policy.history_head;
    let initial_push = &attempt.initial_push.statement;
    let final_push = &attempt.final_push.statement;
    let push_binding_changed = initial_push.repository_id != final_push.repository_id
        || initial_push.pr_number != final_push.pr_number
        || initial_push.head_ref != final_push.head_ref
        || initial_push.candidate != final_push.candidate
        || initial_push.pusher_id != final_push.pusher_id
        || initial_push.delivery_id != final_push.delivery_id
        || initial_push.delivery_guid != final_push.delivery_guid
        || initial_push.delivered_at != final_push.delivered_at
        || initial_push.received_at != final_push.received_at
        || initial_push.ledger_digest != final_push.ledger_digest
        || initial_push.page_chain_digest != final_push.page_chain_digest
        || initial_push.high_water_id != final_push.high_water_id
        || initial_push.watermark_observed_at != final_push.watermark_observed_at
        || initial_push.retention_expires_at != final_push.retention_expires_at;
    if keys.policy_key == keys.push_key
        || attempt.initial_snapshot.main != attempt.final_snapshot.main
        || attempt.initial_snapshot.pull.candidate != attempt.final_snapshot.pull.candidate
        || policy_binding_changed
        || push_binding_changed
    {
        return Err(GateError::RevalidationRace);
    }
    Ok(())
}
