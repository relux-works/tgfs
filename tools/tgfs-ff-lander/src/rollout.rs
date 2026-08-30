use serde::{Deserialize, Serialize};

/// Exact repository merge-setting observation. These fields are assertion-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutSnapshot {
    /// Main before rollout; rollback must never write it.
    pub main: String,
    /// Merge commits remain disabled.
    pub allow_merge_commit: bool,
    /// Squash remains disabled.
    pub allow_squash_merge: bool,
    /// Rebase remains enabled.
    pub allow_rebase_merge: bool,
    /// Exact writable ruleset snapshot.
    pub ruleset: Ruleset,
}

/// Canonical ruleset projection used for exact round-trip assertions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ruleset {
    /// Stable ruleset id.
    pub id: u64,
    /// Canonical JSON of name/target/enforcement/bypass/conditions/rules.
    pub writable_projection: serde_json::Value,
}

/// Mutation plan with a single ruleset write and read-only assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesetPlan {
    /// Exact ruleset id allowed to change.
    pub ruleset_id: u64,
    /// Exact payload to PUT.
    pub payload: serde_json::Value,
    /// Main value to assert without writing.
    pub assert_main: String,
    /// App suspension is required before rollback.
    pub suspend_app_first: bool,
    /// Repository settings are read-only assertions.
    pub write_repository_settings: bool,
    /// Main is never written by rollout/rollback tooling.
    pub write_main: bool,
}

/// Build the only prospective production ruleset shape.
pub fn desired_ruleset(ruleset_id: u64, lander_integration_id: u64) -> Option<Ruleset> {
    if ruleset_id == 0 || lander_integration_id == 0 {
        return None;
    }
    Some(Ruleset {
        id: ruleset_id,
        writable_projection: serde_json::json!({
            "name": "Protect main",
            "target": "branch",
            "enforcement": "active",
            "bypass_actors": [{
                "actor_id": lander_integration_id,
                "actor_type": "Integration",
                "bypass_mode": "always"
            }],
            "conditions": {"ref_name": {
                "include": ["refs/heads/main"], "exclude": []
            }},
            "rules": [
                {"type": "deletion"},
                {"type": "non_fast_forward"},
                {"type": "required_signatures"},
                {"type": "pull_request", "parameters": {
                    "allowed_merge_methods": ["merge"],
                    "dismiss_stale_reviews_on_push": true,
                    "required_approving_review_count": 1,
                    "require_code_owner_review": false,
                    "require_last_push_approval": true,
                    "required_review_thread_resolution": true,
                    "dismissal_restriction": {"enabled": false, "allowed_actors": []},
                    "required_reviewers": []
                }},
                {"type": "required_status_checks", "parameters": {
                    "strict_required_status_checks_policy": true,
                    "do_not_enforce_on_create": false,
                    "required_status_checks": [
                        {"context": "rust-core", "integration_id": 15368},
                        {"context": "secret-scan", "integration_id": 15368}
                    ]
                }}
            ]
        }),
    })
}

/// Create the prospective one-ruleset rollout plan after disposable exact
/// round-trip proof has succeeded.
pub fn rollout_plan(
    before: &RolloutSnapshot,
    desired: Ruleset,
    disposable_desired_observed: &Ruleset,
    disposable_rollback_observed: &Ruleset,
) -> Option<RulesetPlan> {
    if before.ruleset.id != desired.id
        || &desired != disposable_desired_observed
        || &before.ruleset != disposable_rollback_observed
        || before.allow_merge_commit
        || before.allow_squash_merge
        || !before.allow_rebase_merge
    {
        return None;
    }
    Some(RulesetPlan {
        ruleset_id: desired.id,
        payload: desired.writable_projection,
        assert_main: before.main.clone(),
        suspend_app_first: false,
        write_repository_settings: false,
        write_main: false,
    })
}

/// Create a forward-only rollback plan. A legitimate main advance is retained
/// as the assertion value and never replaced with the old snapshot value.
pub fn rollback_plan(before: &RolloutSnapshot, current_main: &str) -> RulesetPlan {
    RulesetPlan {
        ruleset_id: before.ruleset.id,
        payload: before.ruleset.writable_projection.clone(),
        assert_main: current_main.to_owned(),
        suspend_app_first: true,
        write_repository_settings: false,
        write_main: false,
    }
}
