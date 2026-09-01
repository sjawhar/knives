//! The shape `gh` actually returns, not the shape we assumed.
//!
//! Every forge defect found so far passed a suite of hand-written fixtures: only open
//! pull requests were requested, ownership was inferred from a branch name, mergeability
//! was never asked for. Each was invisible because the fixture and the assumption had the
//! same author. This asserts against recorded real output instead.

use std::collections::BTreeSet;

use knives::forge::github::{
    parse_pull_facts, parse_pull_timeline, parse_summaries, parse_sweep, pull_facts_query,
    pull_timeline_query, summary_fields, summary_list_args,
};
use knives::forge::{ChecksSummary, PullRequest, PullSummary, TimelineEventKind};
use serde_json::Value;

const RECORDED_SUMMARIES: &str = include_str!("fixtures/gh_pr_list.json");
const RECORDED_FACTS: &str = include_str!("fixtures/gh_pull_facts.json");
const RECORDED_SWEEP: &str = include_str!("fixtures/gh_sweep.json");
const RECORDED_TIMELINE: &str = include_str!("fixtures/gh_pull_timeline.json");
const SCRUBBED_REPOSITORY: &str = "https://forge.invalid/our-org/recorded-repo";
const REAL_FORGE_HOST: &str = concat!("github", ".com");

fn recorded_repository(recorded: &Value) -> Result<&serde_json::Map<String, Value>, &'static str> {
    recorded
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("repository"))
        .and_then(Value::as_object)
        .ok_or("recorded facts has a repository")
}

fn recorded_fact_numbers() -> Result<Vec<u64>, String> {
    let recorded: Value =
        serde_json::from_str(RECORDED_FACTS).map_err(|error| format!("recorded JSON: {error}"))?;
    Ok(recorded_repository(&recorded)?
        .values()
        .filter_map(|pull| pull.get("number").and_then(Value::as_u64))
        .collect())
}

fn decoded_facts() -> Result<std::collections::BTreeMap<u64, knives::forge::PullFacts>, String> {
    let numbers = recorded_fact_numbers()?;
    parse_pull_facts(RECORDED_FACTS, &numbers)
        .map_err(|error| format!("recorded fact output: {error}"))
}

#[test]
fn every_field_we_request_survives_a_real_payload() {
    let parsed = parse_summaries(RECORDED_SUMMARIES).expect("recorded gh output must deserialise");
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
        parsed.iter().any(|pr| pr
            .base_ref_name
            .as_deref()
            .is_some_and(|base| !base.is_empty())),
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
fn every_summary_field_is_requested() {
    // The failure this prevents: adding a field to PullSummary and forgetting the
    // cheap list query, so it deserialises as its default in the cache forever.
    let fields = serde_json::to_value(PullSummary {
        number: 1,
        state: String::new(),
        review_decision: String::new(),
        head_ref_name: String::new(),
        head_ref_oid: String::new(),
        updated_at: String::new(),
        is_draft: false,
        url: String::new(),
        head_repository_owner: None,
        base_ref_name: Some(String::new()),
        merge_commit: None,
    })
    .expect("PullSummary serialises");
    let held: BTreeSet<&str> = fields
        .as_object()
        .expect("PullSummary serialises to an object")
        .keys()
        .map(String::as_str)
        .collect();
    let requested: BTreeSet<&str> = summary_fields().split(',').map(str::trim).collect();

    assert!(
        held.is_subset(&requested),
        "summary fields missing from the list query: {:#?}",
        held.difference(&requested).collect::<Vec<_>>()
    );
}

#[test]
fn every_fact_field_is_in_the_batch_fragment() {
    // The failure this prevents: adding a live PullRequest field but not asking
    // the batch fragment for it, leaving every status report at the field's default.
    let fields = serde_json::to_value(PullRequest {
        number: 1,
        state: String::new(),
        review_decision: String::new(),
        head_ref_name: String::new(),
        head_ref_oid: String::new(),
        updated_at: String::new(),
        is_draft: false,
        url: String::new(),
        head_repository_owner: None,
        mergeable: Some(String::new()),
        merge_state_status: Some(String::new()),
        base_ref_name: Some(String::new()),
        merge_commit: None,
    })
    .expect("PullRequest serialises");
    let query = pull_facts_query(&[1]);
    let requested: BTreeSet<&str> = query
        .split(|character: char| !character.is_ascii_alphanumeric())
        .collect();

    for field in fields
        .as_object()
        .expect("PullRequest serialises to an object")
        .keys()
    {
        assert!(
            requested.contains(field.as_str()),
            "the facts fragment is missing {field}"
        );
    }
}

#[test]
fn the_list_request_keeps_base_but_not_the_check_rollup() {
    let requested: BTreeSet<&str> = summary_fields().split(',').map(str::trim).collect();
    assert!(requested.contains("baseRefName"));
    assert!(!requested.contains("statusCheckRollup"));
    assert!(!requested.contains("mergeable"));
    assert!(!requested.contains("mergeStateStatus"));
}

#[test]
fn recorded_check_rollups_match_the_batch_decoder() {
    let facts = decoded_facts().expect("recorded fact output decodes");
    let checks: Vec<&ChecksSummary> = facts
        .values()
        .filter_map(|fact| fact.details.checks.as_ref())
        .collect();

    assert!(!checks.is_empty(), "the fixture must carry check rollups");
    assert!(
        checks.iter().any(|checks| checks.ran()),
        "the recording includes a check run"
    );
    assert!(
        checks.iter().any(|checks| !checks.ran()),
        "the recording includes a consulted, never-ran rollup"
    );
    assert!(
        checks.iter().all(|checks| !checks.failing()),
        "recorded rollups must not be falsely classified as failing: {checks:?}"
    );
}

#[test]
fn a_recorded_batch_payload_decodes_every_field_the_query_asks_for() {
    // The defect this prevents: a query field added and the decoder not, so the
    // report reads "nothing to compare" forever while the forge answered.
    let facts = decoded_facts().expect("recorded fact output decodes");
    assert!(!facts.is_empty(), "the fixture must carry pull requests");
    assert!(
        facts
            .values()
            .any(|fact| fact.details.review_predates_head.is_some()),
        "no recorded pull request had a review to compare: {facts:?}"
    );
    assert!(
        facts
            .values()
            .any(|fact| fact.details.checks.as_ref().is_some_and(ChecksSummary::ran)),
        "no recorded pull request had checks: {facts:?}"
    );
    assert!(
        facts.values().any(|fact| fact.newest_comment.is_some()),
        "no recorded pull request had comment activity: {facts:?}"
    );
    assert!(
        facts.values().any(|fact| fact.details.diff.is_some()),
        "no recorded pull request carried diff totals: {facts:?}"
    );
    assert!(
        facts
            .values()
            .any(|fact| fact.details.head_ref_deleted.is_some()),
        "no recorded pull request answered head-ref presence: {facts:?}"
    );
    assert!(
        facts
            .values()
            .any(|fact| fact.details.tip_commit_empty.is_some()),
        "no recorded pull request answered tip emptiness: {facts:?}"
    );

    let query = pull_facts_query(&[1]);
    for field in [
        "number",
        "state",
        "reviewDecision",
        "headRefName",
        "headRefOid",
        "updatedAt",
        "isDraft",
        "url",
        "headRepositoryOwner",
        "baseRefName",
        "mergeable",
        "mergeStateStatus",
        "mergeCommit",
        "additions",
        "deletions",
        "changedFiles",
        "headRef",
        "tree",
        "parents",
        "submittedAt",
        "committedDate",
        "hasNextPage",
        "statusCheckRollup",
        "createdAt",
    ] {
        assert!(query.contains(field), "the query dropped {field}");
        assert!(
            RECORDED_FACTS.contains(field),
            "the recording lacks {field}"
        );
    }
}

#[test]
fn a_recorded_sweep_payload_decodes() {
    let sweep = parse_sweep(RECORDED_SWEEP).expect("recorded sweep output decodes");
    assert!(
        !sweep.entries.is_empty(),
        "the fixture must carry pull requests"
    );
    assert!(
        sweep.entries.iter().all(|entry| entry.number > 0),
        "every sweep entry must have a pull request number"
    );
    assert!(
        sweep
            .entries
            .iter()
            .all(|entry| !entry.updated_at.is_empty()),
        "every sweep entry must have an update time"
    );
    assert!(
        sweep.entries.iter().all(|entry| !entry.state.is_empty()),
        "every sweep entry must have a state"
    );
}

/// Prints the facts query so a real reply can be recorded into
/// `tests/fixtures/gh_pull_facts.json`.
#[test]
#[ignore = "recording utility, not a check; see the doc comment"]
fn print_the_facts_query() {
    println!("{}", pull_facts_query(&[1331, 5116]));
}

/// Prints the sweep query so a real reply can be recorded into
/// `tests/fixtures/gh_sweep.json`.
#[test]
#[ignore = "recording utility, not a check; see the doc comment"]
fn print_the_sweep_query() {
    println!("{}", knives::forge::github::sweep_query());
}

/// Prints the timeline query so a real reply can be recorded into
/// `tests/fixtures/gh_pull_timeline.json`.
#[test]
#[ignore = "recording utility, not a check; see the doc comment"]
fn print_the_timeline_query() {
    println!("{}", pull_timeline_query(1413));
}

#[test]
fn a_recorded_timeline_payload_decodes() {
    let events =
        parse_pull_timeline(RECORDED_TIMELINE, 1413).expect("recorded timeline payload decodes");

    assert!(
        events.iter().any(|event| {
            matches!(
                &event.kind,
                TimelineEventKind::ForcePush { before, after }
                    if [before.commit.as_str(), before.tree.as_str(), after.commit.as_str(), after.tree.as_str()]
                        .into_iter()
                        .all(|oid| oid.len() == 40 && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
            )
        }),
        "the recording must carry full force-push commit and tree ids: {events:?}"
    );
    assert!(
        events.windows(2).any(|events| {
            matches!(
                events,
                [
                    knives::forge::TimelineEvent {
                        kind: TimelineEventKind::HeadDeleted,
                        ..
                    },
                    knives::forge::TimelineEvent {
                        kind: TimelineEventKind::HeadRestored,
                        ..
                    }
                ]
            )
        }),
        "the recording must preserve the delete/restore pair: {events:?}"
    );
}

#[test]
fn the_pull_request_list_argument_array_requests_every_state() {
    let arguments = summary_list_args();
    assert_eq!(arguments.get(2), Some(&"--state"));
    assert_eq!(arguments.get(3), Some(&"all"));
}

#[test]
fn the_recorded_summary_payload_is_scrubbed() {
    assert!(!RECORDED_SUMMARIES.contains(REAL_FORGE_HOST));

    let recorded: Vec<Value> =
        serde_json::from_str(RECORDED_SUMMARIES).expect("recorded JSON is valid");
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
    }
}

#[test]
fn the_recorded_facts_payload_is_scrubbed() {
    assert!(!RECORDED_FACTS.contains(REAL_FORGE_HOST));

    let recorded: Value = serde_json::from_str(RECORDED_FACTS).expect("recorded JSON is valid");
    let pulls = recorded_repository(&recorded).expect("recorded facts has a repository");
    for pull_request in pulls.values() {
        let pull_request = pull_request
            .as_object()
            .expect("recorded pull request is an object");
        if let Some(owner) = pull_request
            .get("headRepositoryOwner")
            .and_then(Value::as_object)
        {
            assert_eq!(owner.get("login").and_then(Value::as_str), Some("our-org"));
        }
        let url = pull_request
            .get("url")
            .and_then(Value::as_str)
            .expect("recorded pull request has a URL");
        assert!(url.starts_with(SCRUBBED_REPOSITORY));
    }
}

#[test]
fn the_recorded_sweep_payload_is_scrubbed() {
    let lower = RECORDED_SWEEP.to_ascii_lowercase();
    for identity in [
        concat!("ha", "wk"),
        concat!("middle", "man"),
        concat!("re", "lay"),
        REAL_FORGE_HOST,
    ] {
        assert!(
            !lower.contains(identity),
            "sweep fixture leaks internal identifier `{identity}`"
        );
    }
}

#[test]
fn the_recorded_timeline_payload_is_scrubbed() {
    let lower = RECORDED_TIMELINE.to_ascii_lowercase();
    for identity in [
        concat!("me", "tr"),
        concat!("ha", "wk"),
        concat!("sjaw", "har"),
        REAL_FORGE_HOST,
    ] {
        assert!(
            !lower.contains(identity),
            "timeline fixture leaks internal identifier `{identity}`"
        );
    }

    let recorded: Value = serde_json::from_str(RECORDED_TIMELINE).expect("recorded JSON is valid");
    let nodes = recorded
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(Value::as_array)
        .expect("recorded timeline has event nodes");
    assert!(
        nodes
            .iter()
            .all(|node| node.get("actor").is_none() && node.get("url").is_none()),
        "the bounded event payload must not retain account or URL fields"
    );
}
