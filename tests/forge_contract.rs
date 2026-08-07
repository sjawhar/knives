//! The shape `gh` actually returns, not the shape we assumed.
//!
//! Every forge defect found so far passed a suite of hand-written fixtures: only open
//! pull requests were requested, ownership was inferred from a branch name, mergeability
//! was never asked for. Each was invisible because the fixture and the assumption had the
//! same author. This asserts against recorded real output instead.

use std::collections::BTreeSet;

use knives::forge::{parse_checks, parse_pull_requests, pull_request_list_args, requested_fields};
use serde_json::Value;

const RECORDED: &str = include_str!("fixtures/gh_pr_list.json");
const SCRUBBED_REPOSITORY: &str = "https://forge.invalid/our-org/recorded-repo";
const REAL_FORGE_HOST: &str = concat!("github", ".com");

#[test]
fn every_field_we_request_survives_a_real_payload() {
    let parsed = parse_pull_requests(RECORDED).expect("recorded gh output must deserialise");
    assert!(!parsed.is_empty(), "the fixture must contain pull requests");

    // Guard against a degenerate or over-scrubbed recording whose typed fields are defaults.
    assert!(parsed.iter().any(|pr| pr.number > 0), "number");
    assert!(parsed.iter().any(|pr| !pr.state.is_empty()), "state");
    assert!(
        parsed.iter().any(|pr| !pr.review_decision.is_empty()),
        "reviewDecision"
    );
    assert!(
        parsed.iter().any(|pr| !pr.head_ref_name.is_empty()),
        "headRefName"
    );
    assert!(
        parsed.iter().any(|pr| !pr.head_ref_oid.is_empty()),
        "headRefOid"
    );
    assert!(
        parsed.iter().any(|pr| !pr.updated_at.is_empty()),
        "updatedAt"
    );
    assert!(parsed.iter().any(|pr| !pr.url.is_empty()), "url");
    assert!(
        parsed.iter().any(|pr| !pr.mergeable.is_empty()),
        "mergeable"
    );
    assert!(
        parsed.iter().any(|pr| !pr.merge_state_status.is_empty()),
        "mergeStateStatus"
    );
    assert!(
        parsed.iter().any(|pr| !pr.base_ref_name.is_empty()),
        "baseRefName"
    );
    assert!(parsed.iter().any(|pr| pr.is_draft), "isDraft");
    assert!(
        parsed.iter().any(|pr| pr.head_repository_owner.is_some()),
        "headRepositoryOwner"
    );
    assert!(
        parsed.iter().any(|pr| pr
            .merge_commit
            .as_ref()
            .is_some_and(|merge| !merge.oid.is_empty())),
        "mergeCommit"
    );
}

#[test]
fn every_pull_request_field_is_requested() {
    // The failure this prevents: adding a field to PullRequest and forgetting PR_FIELDS,
    // so it deserialises as its default and every report quietly reads "not set".
    let parsed = parse_pull_requests(RECORDED).expect("recorded gh output must deserialise");
    let mut held = BTreeSet::new();
    // Unioning records lowers the chance that an optional skipped field weakens this gate,
    // but cannot cover a field that is absent from every recorded pull request.
    for pull_request in parsed {
        let fields = serde_json::to_value(pull_request).expect("PullRequest serialises");
        let fields = fields
            .as_object()
            .expect("PullRequest serialises to an object");
        held.extend(fields.keys().cloned());
    }
    let requested: Vec<&str> = requested_fields().split(',').map(str::trim).collect();

    for field in held {
        assert!(
            requested.contains(&field.as_str()),
            "PR_FIELDS is missing {field}"
        );
    }
}

#[test]
fn the_list_request_keeps_base_but_not_the_check_rollup() {
    let requested: Vec<&str> = requested_fields().split(',').map(str::trim).collect();
    assert!(requested.contains(&"baseRefName"));
    assert!(!requested.contains(&"statusCheckRollup"));
}

#[test]
fn recorded_check_rollups_match_the_per_pull_request_decoder() {
    // Given: every recorded list payload rollup, including its forge-only fields
    let recorded: Vec<Value> = serde_json::from_str(RECORDED).expect("recorded JSON is valid");
    let mut saw_empty = false;

    // When: each rollup is shaped like `gh pr view --json statusCheckRollup` and decoded
    for pull_request in &recorded {
        let rollup = pull_request
            .get("statusCheckRollup")
            .expect("recorded pull request has a rollup");
        let payload = serde_json::json!({"statusCheckRollup": rollup}).to_string();
        let checks = parse_checks(&payload).expect("recorded rollup must deserialise");
        saw_empty |= checks.runs.is_empty();
        assert!(
            !checks.failing(),
            "recorded rollup must not be falsely classified as failing: {checks:?}"
        );
    }

    // Then: the empty rollup stays a consulted, never-ran result
    assert!(saw_empty, "the recording includes an empty rollup");

    // StatusContext is the variant this recording lacks. Reusing one recorded CheckRun keeps
    // its real unknown fields while proving an in-flight conclusion remains non-failing.
    let mut in_flight = recorded
        .first()
        .and_then(|pull_request| pull_request.get("statusCheckRollup"))
        .and_then(Value::as_array)
        .and_then(|rollup| rollup.first())
        .cloned()
        .expect("recorded pull request has a check run");
    *in_flight
        .as_object_mut()
        .and_then(|check| check.get_mut("conclusion"))
        .expect("recorded check run has a conclusion") = Value::String(String::new());
    let payload = serde_json::json!({"statusCheckRollup": [in_flight]}).to_string();
    let checks = parse_checks(&payload).expect("in-flight rollup must deserialise");
    assert!(checks.ran());
    assert!(!checks.failing());
}

#[test]
fn the_pull_request_list_argument_array_requests_every_state() {
    let arguments = pull_request_list_args();
    assert_eq!(arguments.get(2), Some(&"--state"));
    assert_eq!(arguments.get(3), Some(&"all"));
}

#[test]
fn the_recorded_payload_is_scrubbed() {
    assert!(!RECORDED.contains(REAL_FORGE_HOST));

    let recorded: Vec<Value> = serde_json::from_str(RECORDED).expect("recorded JSON is valid");
    for pull_request in recorded {
        let pull_request = pull_request
            .as_object()
            .expect("recorded pull request is an object");
        if let Some(owner) = pull_request
            .get("headRepositoryOwner")
            .and_then(Value::as_object)
        {
            assert_eq!(owner.get("id").and_then(Value::as_str), Some("OWNER-ID"));
            assert_eq!(
                owner.get("name").and_then(Value::as_str),
                Some("Our Organization")
            );
            assert_eq!(owner.get("login").and_then(Value::as_str), Some("our-org"));
        }

        let url = pull_request
            .get("url")
            .and_then(Value::as_str)
            .expect("recorded pull request has a URL");
        assert!(url.starts_with(SCRUBBED_REPOSITORY));

        let checks = pull_request
            .get("statusCheckRollup")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for check in checks {
            if let Some(url) = check
                .as_object()
                .and_then(|check| check.get("detailsUrl"))
                .and_then(Value::as_str)
            {
                assert!(url.starts_with(SCRUBBED_REPOSITORY));
            }
        }
    }
}
