use std::ffi::OsString;

use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

use super::*;
use crate::attestation::PinnedAttestors;
use crate::eligibility::{Attempt, GateError};
use crate::protocol::{Advertisement, ReceiveStatus, encode_lines, fixed_update};

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";
const INTERMEDIATE: &str = "3333333333333333333333333333333333333333";
const NOW: i64 = 1_800_000_000;

struct Fixture {
    attempt: Attempt,
    keys: PinnedAttestors,
    policy_signer: SigningKey,
    push_signer: SigningKey,
}

fn sign<T: Serialize>(signer: &SigningKey, value: &T) -> Vec<u8> {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    signer.sign(&bytes).to_bytes().to_vec()
}

fn one_page<T>(values: Vec<T>) -> PageSet<T> {
    PageSet {
        pages: vec![values],
        next: vec![None],
    }
}

fn fixture() -> Fixture {
    let policy_signer = SigningKey::from_bytes(&[1; 32]);
    let push_signer = SigningKey::from_bytes(&[2; 32]);
    let policy = PolicyStatement {
        repository_id: FIXED_REPOSITORY_ID,
        reference: "refs/heads/main".into(),
        configuration_digest: "config-v7".into(),
        policy_digest: "policy-v7".into(),
        bypass_digest: "bypass-v7".into(),
        source_types: vec![
            "Repository".into(),
            "Organization".into(),
            "Enterprise".into(),
        ],
        ruleset_ids: vec![101, 202, 303],
        ruleset_updated_at: vec![NOW - 30, NOW - 29, NOW - 28],
        coverage_complete: true,
        history_head: "ruleset-history-7".into(),
        issued_at: NOW - 5,
        expires_at: NOW + 55,
    };
    let signed_policy = SignedPolicyStatement {
        signature: sign(&policy_signer, &policy),
        statement: policy,
    };
    let push = PushStatement {
        repository_id: FIXED_REPOSITORY_ID,
        pr_number: 7,
        head_ref: "signed-head".into(),
        candidate: NEW.into(),
        pusher_id: 20,
        delivery_id: "delivery-7".into(),
        delivery_guid: "guid-7".into(),
        delivered_at: NOW - 20,
        received_at: NOW - 19,
        ledger_digest: "ledger".into(),
        page_chain_digest: "pages".into(),
        high_water_id: 900,
        watermark_observed_at: NOW - 6,
        retention_expires_at: NOW + 3_600,
        reconciliation_complete: true,
        issued_at: NOW - 5,
        expires_at: NOW + 55,
    };
    let signed_push = SignedPushStatement {
        signature: sign(&push_signer, &push),
        statement: push,
    };
    let snapshot = Snapshot {
        visible_rules_complete: true,
        visible_rules_active: true,
        main: OLD.into(),
        pull: PullRequest {
            open: true,
            draft: false,
            base_repository_id: FIXED_REPOSITORY_ID,
            head_repository_id: FIXED_REPOSITORY_ID,
            base_ref: "main".into(),
            head_ref: "signed-head".into(),
            candidate: NEW.into(),
            author_id: 10,
        },
        compare: CompareState::Ahead,
        changed_paths: one_page(vec!["src/lib.rs".into()]),
        commits: one_page(vec![CommitEvidence {
            sha: NEW.into(),
            github_verified: true,
            github_reason: "valid".into(),
            human_authored: true,
            ssh_gpgsig: true,
            ssh_verified: true,
            principal_matched: true,
        }]),
        reviews: one_page(vec![Review {
            id: 1,
            reviewer_id: 30,
            commit_id: NEW.into(),
            state: ReviewState::Approved,
            submitted_at: NOW - 10,
            permission: "write".into(),
        }]),
        threads: one_page(vec![Thread {
            id: "thread-1".into(),
            resolved: true,
        }]),
        checks: one_page(vec![
            CheckRun {
                id: 40,
                name: "rust-core".into(),
                app_id: 15_368,
                status: "completed".into(),
                conclusion: "success".into(),
                head_sha: NEW.into(),
                completed_at: NOW - 9,
            },
            CheckRun {
                id: 41,
                name: "secret-scan".into(),
                app_id: 15_368,
                status: "completed".into(),
                conclusion: "success".into(),
                head_sha: NEW.into(),
                completed_at: NOW - 8,
            },
        ]),
        statuses: one_page(vec![]),
    };
    Fixture {
        attempt: Attempt {
            initial_snapshot: snapshot.clone(),
            final_snapshot: snapshot,
            initial_policy: signed_policy.clone(),
            final_policy: signed_policy,
            initial_push: signed_push.clone(),
            final_push: signed_push,
        },
        keys: PinnedAttestors::test_new(
            policy_signer.verifying_key(),
            push_signer.verifying_key(),
            "config-v7".into(),
            "policy-v7".into(),
            "bypass-v7".into(),
        ),
        policy_signer,
        push_signer,
    }
}

#[derive(Clone, Copy)]
enum Failure {
    None,
    Read,
    AuditReady,
    Mint,
    Advertise,
    Send,
    PostRead,
    AuditAppend,
}

struct Backend {
    attempt: Attempt,
    failure: Failure,
    advertised: String,
    post_main: String,
    receive_ok: bool,
    reads: usize,
    mints: usize,
    sends: usize,
    audits: usize,
    request: Vec<u8>,
}

impl Backend {
    fn valid(attempt: Attempt) -> Self {
        Self {
            attempt,
            failure: Failure::None,
            advertised: OLD.into(),
            post_main: NEW.into(),
            receive_ok: true,
            reads: 0,
            mints: 0,
            sends: 0,
            audits: 0,
            request: Vec::new(),
        }
    }
}

impl FixedBackend for Backend {
    fn now_unix(&self) -> i64 {
        NOW
    }

    fn read_attempt(&mut self, _pr: u64) -> Result<Attempt, BackendError> {
        self.reads += 1;
        if matches!(self.failure, Failure::Read) {
            Err(BackendError)
        } else {
            Ok(self.attempt.clone())
        }
    }

    fn audit_ready(&mut self) -> Result<(), BackendError> {
        if matches!(self.failure, Failure::AuditReady) {
            Err(BackendError)
        } else {
            Ok(())
        }
    }

    fn mint_short_lived_token(&mut self) -> Result<SecretToken, BackendError> {
        self.mints += 1;
        if matches!(self.failure, Failure::Mint) {
            Err(BackendError)
        } else {
            Ok(SecretToken(b"in-memory-only".to_vec()))
        }
    }

    fn advertise_receive_pack(&mut self, _token: &SecretToken) -> Result<Vec<u8>, BackendError> {
        if matches!(self.failure, Failure::Advertise) {
            return Err(BackendError);
        }
        let first = format!(
            "{} refs/heads/main\0report-status delete-refs\n",
            self.advertised
        );
        Ok(encode_lines(&[first.as_bytes()]))
    }

    fn send_receive_pack(
        &mut self,
        _token: &SecretToken,
        request: &[u8],
    ) -> Result<Vec<u8>, BackendError> {
        self.sends += 1;
        self.request = request.to_vec();
        if matches!(self.failure, Failure::Send) {
            return Err(BackendError);
        }
        if self.receive_ok {
            Ok(encode_lines(&[b"unpack ok\n", b"ok refs/heads/main\n"]))
        } else {
            Ok(encode_lines(&[
                b"unpack ok\n",
                b"ng refs/heads/main stale info\n",
            ]))
        }
    }

    fn read_main(&mut self) -> Result<String, BackendError> {
        if matches!(self.failure, Failure::PostRead) {
            Err(BackendError)
        } else {
            Ok(self.post_main.clone())
        }
    }

    fn append_audit(&mut self, _record: AuditRecord) -> Result<(), BackendError> {
        self.audits += 1;
        if matches!(self.failure, Failure::AuditAppend) {
            Err(BackendError)
        } else {
            Ok(())
        }
    }
}

fn argv(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

fn execute(fixture: &Fixture, backend: &mut Backend) -> i32 {
    run(
        argv(&["tgfs-ff-lander", "land", "--pr", "7"]),
        backend,
        &fixture.keys,
    )
}

fn resign_policy(fixture: &mut Fixture) {
    fixture.attempt.initial_policy.signature = sign(
        &fixture.policy_signer,
        &fixture.attempt.initial_policy.statement,
    );
    fixture.attempt.final_policy = fixture.attempt.initial_policy.clone();
}

fn resign_push(fixture: &mut Fixture) {
    fixture.attempt.initial_push.signature = sign(
        &fixture.push_signer,
        &fixture.attempt.initial_push.statement,
    );
    fixture.attempt.final_push = fixture.attempt.initial_push.clone();
}

#[test]
fn canonical_entry_lands_with_one_fixed_empty_pack_update() {
    let fixture = fixture();
    let mut backend = Backend::valid(fixture.attempt.clone());
    assert_eq!(execute(&fixture, &mut backend), 0);
    assert_eq!(
        (backend.reads, backend.mints, backend.sends, backend.audits),
        (1, 1, 1, 1)
    );
    assert_eq!(backend.request, fixed_update(OLD, NEW).unwrap_or_default());
    assert!(
        backend
            .request
            .windows(b"refs/heads/main".len())
            .any(|w| w == b"refs/heads/main")
    );
    assert!(!backend.request.windows(5).any(|w| w == b"force"));
    assert!(!backend.request.windows(11).any(|w| w == b"push-option"));
    assert!(
        backend
            .request
            .windows(b"refs/heads/main\0report-status\n".len())
            .any(|window| window == b"refs/heads/main\0report-status\n")
    );
    assert!(!backend.request.windows(2).any(|window| window == b"\0 "));
    assert_eq!(
        &backend.request[backend.request.len() - 32..backend.request.len() - 20],
        b"PACK\0\0\0\x02\0\0\0\0"
    );
}

#[test]
fn advertisement_and_report_status_parsers_reject_protocol_mutants() {
    let first = format!("{OLD} refs/heads/main\0report-status delete-refs\n");
    let service = encode_lines(&[b"# service=git-receive-pack\n"]);
    let mut actual_http_advertisement = service[..service.len() - 4].to_vec();
    actual_http_advertisement.extend_from_slice(b"0000");
    actual_http_advertisement.extend_from_slice(&encode_lines(&[first.as_bytes()]));
    assert_eq!(
        Advertisement::parse(&actual_http_advertisement).map(|value| value.main),
        Ok(OLD.into())
    );

    let duplicate = encode_lines(&[first.as_bytes(), first.as_bytes()]);
    assert!(Advertisement::parse(&duplicate).is_err());
    let duplicate_other = encode_lines(&[
        first.as_bytes(),
        format!("{NEW} refs/heads/topic\n").as_bytes(),
        format!("{NEW} refs/heads/topic\n").as_bytes(),
    ]);
    assert!(Advertisement::parse(&duplicate_other).is_err());
    let late_capabilities = encode_lines(&[
        first.as_bytes(),
        format!("{NEW} refs/heads/topic\0report-status\n").as_bytes(),
    ]);
    assert!(Advertisement::parse(&late_capabilities).is_err());
    let no_status = format!("{OLD} refs/heads/main\0delete-refs\n");
    assert!(Advertisement::parse(&encode_lines(&[no_status.as_bytes()])).is_err());
    assert!(Advertisement::parse(&encode_lines(&[first.as_bytes()])[..12]).is_err());
    assert!(
        ReceiveStatus::parse(&encode_lines(&[
            b"unpack ok\n",
            b"ok refs/heads/main\n",
            b"ok refs/heads/other\n"
        ]))
        .is_err()
    );
}

#[test]
fn alternate_and_noncanonical_argv_never_read_or_mint() {
    let fixture = fixture();
    let cases = [
        vec!["tgfs-ff-lander", "land", "--pr", "0"],
        vec!["tgfs-ff-lander", "land", "--pr", "07"],
        vec!["tgfs-ff-lander", "land", "--repo", "other"],
        vec!["tgfs-ff-lander", "land", "--pr", "7", "--force"],
        vec!["tgfs-ff-lander", "push", "--pr", "7"],
        vec!["tgfs-ff-lander", "land", "--sha", NEW],
    ];
    for case in cases {
        let mut backend = Backend::valid(fixture.attempt.clone());
        assert_eq!(run(argv(&case), &mut backend, &fixture.keys), EX_USAGE);
        assert_eq!((backend.reads, backend.mints, backend.sends), (0, 0, 0));
    }
}

#[test]
fn every_read_gate_failure_prevents_token_mint() {
    type FixtureMutation = Box<dyn Fn(&mut Fixture)>;
    let mut mutations: Vec<FixtureMutation> = vec![
        Box::new(|f| f.attempt.initial_snapshot.pull.head_repository_id += 1),
        Box::new(|f| f.attempt.initial_snapshot.compare = CompareState::Diverged),
        Box::new(|f| {
            f.attempt.initial_snapshot.changed_paths.pages[0][0] = ".github/workflows/ci.yml".into()
        }),
        Box::new(|f| f.attempt.initial_snapshot.commits.pages[0][0].ssh_verified = false),
        Box::new(|f| f.attempt.initial_snapshot.reviews.pages[0][0].reviewer_id = 20),
        Box::new(|f| f.attempt.initial_snapshot.threads.pages[0][0].resolved = false),
        Box::new(|f| f.attempt.initial_snapshot.checks.pages[0][0].app_id = 1),
        Box::new(|f| {
            f.attempt.initial_snapshot.statuses.pages[0].push(StatusContext {
                id: 8,
                context: "rust-core".into(),
            })
        }),
        Box::new(|f| f.attempt.initial_snapshot.reviews.next = vec![Some("lost-page".into())]),
    ];
    for mutate in mutations.drain(..) {
        let mut fixture = fixture();
        mutate(&mut fixture);
        let mut backend = Backend::valid(fixture.attempt.clone());
        assert_eq!(execute(&fixture, &mut backend), EX_REFUSED);
        assert_eq!((backend.mints, backend.sends), (0, 0));
    }
}

#[test]
fn forged_stale_partial_and_self_key_attestations_are_refused_before_mint() {
    let mut cases = Vec::new();
    let mut forged = fixture();
    forged.attempt.initial_policy.signature[0] ^= 1;
    cases.push(forged);
    let mut stale = fixture();
    stale.attempt.initial_policy.statement.issued_at = NOW - 61;
    resign_policy(&mut stale);
    cases.push(stale);
    let mut partial = fixture();
    partial
        .attempt
        .initial_push
        .statement
        .reconciliation_complete = false;
    resign_push(&mut partial);
    cases.push(partial);
    let mut incomplete_policy_sources = fixture();
    incomplete_policy_sources
        .attempt
        .initial_policy
        .statement
        .ruleset_ids
        .pop();
    resign_policy(&mut incomplete_policy_sources);
    cases.push(incomplete_policy_sources);
    let mut expired_retention = fixture();
    expired_retention
        .attempt
        .initial_push
        .statement
        .retention_expires_at = NOW - 1;
    resign_push(&mut expired_retention);
    cases.push(expired_retention);
    let mut impossible_watermark = fixture();
    impossible_watermark
        .attempt
        .initial_push
        .statement
        .watermark_observed_at = NOW - 25;
    resign_push(&mut impossible_watermark);
    cases.push(impossible_watermark);
    let mut unknown_source = fixture();
    unknown_source
        .attempt
        .initial_policy
        .statement
        .source_types
        .push("Unknown".into());
    resign_policy(&mut unknown_source);
    cases.push(unknown_source);
    let mut self_minted = fixture();
    let attacker = SigningKey::from_bytes(&[3; 32]);
    self_minted.attempt.initial_push.signature =
        sign(&attacker, &self_minted.attempt.initial_push.statement);
    self_minted.attempt.final_push = self_minted.attempt.initial_push.clone();
    cases.push(self_minted);
    for fixture in cases {
        let mut backend = Backend::valid(fixture.attempt.clone());
        assert_eq!(execute(&fixture, &mut backend), EX_REFUSED);
        assert_eq!(backend.mints, 0);
    }
}

#[test]
fn same_lineage_race_before_advertisement_and_stale_old_after_it_cannot_move() {
    let fixture = fixture();
    let mut before_advertisement = Backend::valid(fixture.attempt.clone());
    before_advertisement.advertised = INTERMEDIATE.into();
    before_advertisement.post_main = INTERMEDIATE.into();
    assert_eq!(execute(&fixture, &mut before_advertisement), EX_REFUSED);
    assert_eq!(
        (before_advertisement.mints, before_advertisement.sends),
        (1, 0)
    );

    let mut after_advertisement = Backend::valid(fixture.attempt.clone());
    after_advertisement.receive_ok = false;
    after_advertisement.post_main = INTERMEDIATE.into();
    assert_eq!(execute(&fixture, &mut after_advertisement), EX_REFUSED);
    assert_eq!(after_advertisement.sends, 1);
    assert_eq!(after_advertisement.audits, 0);
}

#[test]
fn revalidation_change_and_duplicate_check_refuse_before_mint() {
    let mut raced = fixture();
    raced.attempt.final_snapshot.main = INTERMEDIATE.into();
    let mut backend = Backend::valid(raced.attempt.clone());
    assert_eq!(execute(&raced, &mut backend), EX_REFUSED);
    assert_eq!(backend.mints, 0);

    let mut duplicate = fixture();
    let duplicate_check = duplicate.attempt.initial_snapshot.checks.pages[0][0].clone();
    duplicate.attempt.initial_snapshot.checks.pages[0].push(duplicate_check);
    let mut backend = Backend::valid(duplicate.attempt.clone());
    assert_eq!(execute(&duplicate, &mut backend), EX_REFUSED);
    assert_eq!(backend.mints, 0);
}

#[test]
fn malformed_advertisement_receiver_or_exact_post_read_refuses_without_fallback() {
    let fixture = fixture();
    for failure in [Failure::Advertise, Failure::Send, Failure::PostRead] {
        let mut backend = Backend::valid(fixture.attempt.clone());
        backend.failure = failure;
        assert_eq!(execute(&fixture, &mut backend), EX_REFUSED);
        assert!(backend.sends <= 1);
        assert_eq!(backend.audits, 0);
    }
    let mut wrong_post = Backend::valid(fixture.attempt.clone());
    wrong_post.post_main = INTERMEDIATE.into();
    assert_eq!(execute(&fixture, &mut wrong_post), EX_REFUSED);
    assert_eq!(wrong_post.sends, 1);
}

#[test]
fn read_audit_and_broker_failures_are_closed_refusals() {
    let fixture = fixture();
    for failure in [
        Failure::Read,
        Failure::AuditReady,
        Failure::Mint,
        Failure::AuditAppend,
    ] {
        let mut backend = Backend::valid(fixture.attempt.clone());
        backend.failure = failure;
        assert_eq!(execute(&fixture, &mut backend), EX_REFUSED);
        assert!(backend.sends <= 1);
    }
}

#[test]
fn later_changes_requested_overrides_approval() {
    let mut fixture = fixture();
    fixture.attempt.initial_snapshot.reviews.pages[0].push(Review {
        id: 2,
        reviewer_id: 30,
        commit_id: NEW.into(),
        state: ReviewState::ChangesRequested,
        submitted_at: NOW - 6,
        permission: "write".into(),
    });
    let mut backend = Backend::valid(fixture.attempt.clone());
    assert_eq!(execute(&fixture, &mut backend), EX_REFUSED);
    assert_eq!(backend.mints, 0);
}

#[test]
fn rollout_and_rollback_are_ruleset_only_and_forward_only() {
    let before = RolloutSnapshot {
        main: OLD.into(),
        allow_merge_commit: false,
        allow_squash_merge: false,
        allow_rebase_merge: true,
        ruleset: Ruleset {
            id: 20_670_428,
            writable_projection: serde_json::json!({"old": true}),
        },
    };
    let desired = desired_ruleset(20_670_428, 765).unwrap_or(Ruleset {
        id: 0,
        writable_projection: serde_json::Value::Null,
    });
    let mut normalized = desired.clone();
    normalized.writable_projection["enforcement"] = serde_json::json!("evaluate");
    assert!(rollout_plan(&before, desired.clone(), &normalized, &before.ruleset).is_none());
    let rollout = rollout_plan(&before, desired.clone(), &desired, &before.ruleset);
    assert!(rollout.is_some());
    let rollout = rollout.unwrap_or(RulesetPlan {
        ruleset_id: 0,
        payload: serde_json::Value::Null,
        assert_main: String::new(),
        suspend_app_first: true,
        write_repository_settings: true,
        write_main: true,
    });
    assert!(!rollout.suspend_app_first);
    assert!(!rollout.write_repository_settings);
    assert!(!rollout.write_main);
    let rollback = rollback_plan(&before, INTERMEDIATE);
    assert!(rollback.suspend_app_first);
    assert_eq!(rollback.assert_main, INTERMEDIATE);
    assert_eq!(rollback.payload, serde_json::json!({"old": true}));
    assert!(!rollback.write_repository_settings);
    assert!(!rollback.write_main);
}

#[test]
fn production_call_site_rejects_all_extra_surfaces_with_usage() {
    assert_eq!(
        production_main(argv(&["/app/tgfs-ff-lander", "land", "--pr", "7", "--url"])),
        EX_USAGE
    );
    assert_eq!(
        production_main(argv(&["/app/tgfs-ff-lander", "land", "--request", "body"])),
        EX_USAGE
    );
    assert_eq!(
        production_main(argv(&["/app/tgfs-ff-lander", "land", "--pr", "7"])),
        EX_REFUSED
    );
}

#[test]
fn precise_gate_errors_are_stable_for_security_audits() {
    let valid = fixture();
    assert_eq!(
        eligibility::verify_attempt(&valid.attempt, 7, FIXED_REPOSITORY_ID, NOW, &valid.keys),
        Ok(())
    );
    let mut partial = fixture();
    partial.attempt.final_snapshot.checks.next[0] = Some("more".into());
    assert_eq!(
        eligibility::verify_attempt(&partial.attempt, 7, FIXED_REPOSITORY_ID, NOW, &partial.keys),
        Err(GateError::PartialRead)
    );
}

#[test]
fn independently_refreshed_attestations_keep_the_same_bound_evidence() {
    let mut refreshed = fixture();
    refreshed.attempt.final_policy.statement.issued_at += 1;
    refreshed.attempt.final_policy.statement.expires_at += 1;
    refreshed.attempt.final_policy.signature = sign(
        &refreshed.policy_signer,
        &refreshed.attempt.final_policy.statement,
    );
    refreshed.attempt.final_push.statement.issued_at += 1;
    refreshed.attempt.final_push.statement.expires_at += 1;
    refreshed.attempt.final_push.signature = sign(
        &refreshed.push_signer,
        &refreshed.attempt.final_push.statement,
    );
    assert_eq!(
        eligibility::verify_attempt(
            &refreshed.attempt,
            7,
            FIXED_REPOSITORY_ID,
            NOW,
            &refreshed.keys,
        ),
        Ok(())
    );
}
