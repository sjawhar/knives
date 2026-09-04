//! `knives sync`: what the forge says about pull requests, recorded once per move.
//!
//! Fails closed when the facts batch fails; a listed state wins and a vanished
//! number is answered by the batch; each advance and the eventual merge is one
//! ledger event, never repeated across runs.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
#[path = "common/pulls.rs"]
mod pulls;

use knives::commands::sync;
use knives::config::RepoEntry;
use knives::forge::PullRequest;
use knives::ids::BranchName;
use knives::store::Store;
use lab::lab_entry;
use pulls::pull_request_with_head;
use std::collections::BTreeMap;

#[test]
fn sync_fails_closed_when_the_facts_batch_fails() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(42, "OPEN", "feat/alpha", "head-42"),
        )]),
        fail_facts: true,
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("store");
    let scribe = knives::ledger::Scribe::new(
        knives::ledger::Ledger::at(state.path().join("ledger")),
        name,
        lab.work,
        "a-test".to_owned(),
    );

    let report = sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("sync report");

    assert!(report.rows.is_empty(), "was: {report:?}");
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("pull request state unavailable")),
        "was: {report:?}"
    );
    assert_eq!(sync::exit_for(&report), knives::cli::Exit::Incomplete);
}

#[test]
fn a_listed_state_wins_and_a_vanished_number_is_answered_by_the_batch() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 42);
    lab.publish_pull("feat/alpha", 43);
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(42, "OPEN", "feat/alpha", "head-42"),
        )]),
        vanished_states: BTreeMap::from([(42, "MERGED".to_owned()), (43, "MERGED".to_owned())]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("store");
    store.record_pull_head(&name, 43, "previous");
    let scribe = knives::ledger::Scribe::new(
        knives::ledger::Ledger::at(state.path().join("ledger")),
        name,
        lab.work,
        "a-test".to_owned(),
    );

    let report = sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("sync report");

    assert_eq!(
        report
            .rows
            .iter()
            .find(|row| row.number == 42)
            .map(|row| row.state),
        Some(sync::PullState::New),
        "the listed OPEN state was overwritten: {report:?}"
    );
    assert_eq!(
        report
            .rows
            .iter()
            .find(|row| row.number == 43)
            .map(|row| row.state),
        Some(sync::PullState::Merged),
        "the vanished pull did not arrive in the one batch: {report:?}"
    );
    assert!(report.problems.is_empty(), "was: {report:?}");
}

#[test]
fn sync_records_one_event_for_each_pull_request_that_moved() {
    // Given: three tracked pull requests that moved and one that did not.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = RepoEntry::new(
        lab.upstream.display().to_string(),
        lab.work.display().to_string(),
    );
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state) in [
        (10, "feat/merged", "MERGED"),
        (11, "feat/closed", "CLOSED"),
        (12, "feat/moved", "OPEN"),
        (13, "feat/still", "OPEN"),
    ] {
        let _ = pull_requests.insert(
            BranchName::new(branch),
            PullRequest {
                head_ref_oid: format!("head-{number}"),
                updated_at: "2026-08-15T00:00:00Z".to_owned(),
                ..pulls::pull_request(number, state, branch)
            },
        );
    }
    let forge = knives::forge::fake::FakeForge {
        pull_requests,
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    // Every one of them was seen before: a first sighting is recorded silently,
    // whatever the forge already did to it, so only prior sightings can move.
    store.record_pull_head(&name, 10, "head-10");
    store.record_pull_head(&name, 11, "head-11");
    store.record_pull_head(&name, 12, "older");
    store.record_pull_head(&name, 13, "head-13");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work,
        "ses_fff688".to_owned(),
    );

    // When: sync classifies them.
    let report = sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("sync report");
    assert_eq!(report.rows.len(), 4, "was: {report:?}");

    // Then: exactly the moved pulls are events, each under the tracked branch.
    let entries = ledger.entries().expect("read ledger");
    let recorded: Vec<(Option<&str>, &str)> = entries
        .iter()
        .map(|entry| (entry.subject.as_deref(), entry.text.as_str()))
        .collect();
    assert_eq!(
        recorded,
        [
            (Some("feat/merged"), "#10 merged"),
            (Some("feat/closed"), "#11 closed"),
            (Some("feat/moved"), "#12 advanced to head-12"),
        ],
        "was: {entries:?}"
    );
    assert!(entries.iter().all(|entry| entry.owner == "ses_fff688"));
    assert!(
        entries
            .iter()
            .all(|entry| entry.kind == knives::ledger::Kind::Event),
        "sync observed these; it did not assert them"
    );
}

#[test]
fn sync_records_a_settled_pull_request_once_across_repeated_runs() {
    // Given: a merged pull request that remains listed by the forge.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(10, "MERGED", "feat/alpha", "head-10"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    // Seen while open, so the merge is a transition rather than a first sighting.
    store.record_pull_head(&name, 10, "head-10");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe =
        knives::ledger::Scribe::new(ledger.clone(), name, lab.work, "ses_fff688".to_owned());

    // When: the same settled pull request is seen twice.
    sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("first sync");
    sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&forge),
        scribe: &scribe,
        cache: None,
    })
    .expect("second sync");

    // Then: its settled transition remains one fact, not one fact per sync run.
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.text == "#10 merged")
            .count(),
        1,
        "was: {entries:?}"
    );
}

#[test]
fn sync_records_an_advanced_pull_request_then_its_merge() {
    // Given: a tracked pull request whose head advanced before the forge reports it merged.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let advanced = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(12, "OPEN", "feat/alpha", "head-12"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let merged = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(12, "MERGED", "feat/alpha", "head-12"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.record_pull_head(&name, 12, "older");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe =
        knives::ledger::Scribe::new(ledger.clone(), name, lab.work, "ses_fff688".to_owned());

    // When: the head advances, then the forge marks that same pull request merged.
    sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&advanced),
        scribe: &scribe,
        cache: None,
    })
    .expect("advanced sync");
    sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&merged),
        scribe: &scribe,
        cache: None,
    })
    .expect("merged sync");

    // Then: both distinct transitions remain in the ledger in observation order.
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["#12 advanced to head-12", "#12 merged"],
        "was: {entries:?}"
    );
}

#[test]
fn sync_records_each_consecutive_advance() {
    // Given: an open pull request whose head changes twice between sync runs.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let first_advance = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(12, "OPEN", "feat/alpha", "head-b"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let second_advance = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pull_request_with_head(12, "OPEN", "feat/alpha", "head-c"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.record_pull_head(&name, 12, "head-a");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe =
        knives::ledger::Scribe::new(ledger.clone(), name, lab.work, "ses_fff688".to_owned());

    // When: the pull request advances from A to B, then from B to C.
    sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&first_advance),
        scribe: &scribe,
        cache: None,
    })
    .expect("first advance");
    sync::sync_repo(sync::SyncInput {
        fork: &fork,
        store: &mut store,
        forge: Some(&second_advance),
        scribe: &scribe,
        cache: None,
    })
    .expect("second advance");

    // Then: both changed heads are recorded as distinct advances.
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["#12 advanced to head-b", "#12 advanced to head-c"],
        "was: {entries:?}"
    );
}
