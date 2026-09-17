//! A fork branch states the non-fork alternative it rejected before it exists.
//!
//! `start` refuses a new branch with no placement verdict and asks the forcing
//! question; a verdict on the command line is recorded on the branch as a
//! `placement:` note, and a `CONSUMER` verdict starts nothing. `release
//! include` and `release advance --from` read the note back: a branch no
//! composition ever carried needs one that is not `CONSUMER`, while a member
//! some cut or edit recorded is grandfathered.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ledger::{Kind, Ledger};
use knives::placement::{CONSUMER_REFUSAL, forcing_question, missing_member_refusal};
use lab::{
    Lab, home_after_first_cut, knives_command, knives_release, placement_file, release_parents,
    release_test_home, state_placement,
};

/// `knives start <branch> --repo demo --why test [extra…]` without the lab
/// helper's default verdict, so a test states exactly what the command gets.
fn start(
    lab: &Lab,
    home: &tempfile::TempDir,
    branch: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut args = vec!["--text", "start", branch, "--repo", "demo", "--why", "test"];
    args.extend_from_slice(extra);
    knives_command(&lab.work, home.path(), lab.temp_path(), &args)
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("run knives start")
}

fn ledger(home: &tempfile::TempDir) -> Ledger {
    Ledger::at(home.path().join("ledger").join("demo"))
}

#[test]
fn a_new_branch_without_a_verdict_is_refused_with_the_forcing_question() {
    // Given: a registered fork and a branch name nothing of ours carries.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    // When: it is started with a reason but no placement.
    let output = start(&lab, &home, "feat/gamma", &[]);

    // Then: refused verbatim, nothing claimed, no workspace, no ledger entry.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        forcing_question("feat/gamma")
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "a refused start opened a workspace"
    );
    assert!(
        ledger(&home).entries().expect("read ledger").is_empty(),
        "a refused start wrote to the ledger"
    );
    let state = std::fs::read_to_string(home.path().join("state.json")).unwrap_or_default();
    assert!(
        !state.contains("feat/gamma"),
        "a refused start claimed: {state}"
    );
}

#[test]
fn a_verdict_on_the_command_line_is_recorded_on_the_branch() {
    // Given: a verdict file the red-team produced.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let verdict = placement_file(&home, "UPSTREAM");

    // When: the branch is started with it.
    let output = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", verdict.to_str().expect("utf-8 path")],
    );

    // Then: the workspace opens and the ledger carries the verdict as a note
    // after the claim event, marked so the release verbs and `gh` find it.
    assert!(output.status.success(), "{output:?}");
    let entries = ledger(&home).entries().expect("read ledger");
    assert_eq!(entries.len(), 2, "was: {entries:?}");
    assert_eq!(entries[1].kind, Kind::Note);
    assert_eq!(entries[1].subject.as_deref(), Some("feat/gamma"));
    assert!(
        entries[1]
            .text
            .starts_with("placement: verdict: UPSTREAM\n"),
        "was: {}",
        entries[1].text
    );
    assert!(
        entries[1].text.contains("judge: lab-red-team"),
        "the whole file is kept: {}",
        entries[1].text
    );
}

#[test]
fn a_consumer_verdict_starts_no_branch() {
    // A verdict that says the effect belongs in the consumer answers the forcing
    // question with "no fork"; a branch started on it would contradict it.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let verdict = placement_file(&home, "CONSUMER");

    let output = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", verdict.to_str().expect("utf-8 path")],
    );

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(CONSUMER_REFUSAL), "{stderr}");
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "a consumer verdict opened a workspace"
    );
}

#[test]
fn a_verdict_file_without_a_verdict_line_is_refused_before_anything_happens() {
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let file = home.path().join("no-verdict.md");
    std::fs::write(&file, "alternative: none considered\n").expect("write file");

    let output = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", file.to_str().expect("utf-8 path")],
    );

    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("verdict: CONSUMER | FORK | UPSTREAM"),
        "{stderr}"
    );
    assert!(
        !lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "an unreadable verdict opened a workspace"
    );
}

#[test]
fn an_existing_branch_is_continued_without_a_verdict() {
    // The gate is on creation: a branch already on one of our remotes was
    // started before, and continuing it asks nothing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "forget", "feat/alpha"]);
    let (home, _consumer) = release_test_home(&lab);

    let output = start(&lab, &home, "feat/alpha", &[]);

    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("feat/alpha's tip"),
        "{output:?}"
    );
}

#[test]
fn a_recorded_verdict_lets_the_branch_be_started_again_without_the_file() {
    // Given: a branch started with a verdict, then handed back before it was
    // ever pushed — so nothing of ours names it and its workspace is gone.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let verdict = placement_file(&home, "FORK");
    let first = start(
        &lab,
        &home,
        "feat/gamma",
        &["--placement", verdict.to_str().expect("utf-8 path")],
    );
    assert!(first.status.success(), "{first:?}");
    let finished = knives_command(
        &lab.work,
        home.path(),
        lab.temp_path(),
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    )
    .env("KNIVES_OWNER", "agent-one")
    .output()
    .expect("run finish");
    assert!(finished.status.success(), "{finished:?}");

    // When: it is started again with no file.
    let again = start(&lab, &home, "feat/gamma", &[]);

    // Then: the ledger's note answers for it.
    assert!(again.status.success(), "{again:?}");
    assert!(
        lab.work
            .parent()
            .expect("parent")
            .join("feat-gamma")
            .exists(),
        "the workspace was not rebuilt"
    );
}

#[test]
fn include_refuses_a_new_member_without_a_verdict_and_a_consumer_one() {
    // Given: a cut release and a branch no composition has carried.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: included with no verdict behind it.
    let unplaced = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused with the member text, and the release is untouched.
    assert_eq!(unplaced.status.code(), Some(3), "{unplaced:?}");
    assert_eq!(
        String::from_utf8_lossy(&unplaced.stdout).trim_end(),
        format!("demo: {}", missing_member_refusal("feat/gamma"))
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);

    // When: the verdict says the change belongs in the consumer.
    state_placement(&lab, &home, "feat/gamma", "CONSUMER");
    let consumer = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: refused with the consumer text.
    assert_eq!(consumer.status.code(), Some(3), "{consumer:?}");
    assert_eq!(
        String::from_utf8_lossy(&consumer.stdout).trim_end(),
        format!("demo: {CONSUMER_REFUSAL}")
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04"), before);

    // When: a newer verdict rules it a fork member.
    state_placement(&lab, &home, "feat/gamma", "FORK");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: it joins.
    assert!(included.status.success(), "{included:?}");
    assert_eq!(
        release_parents(&lab, "release/2026-08-04").len(),
        before.len() + 1
    );
}

#[test]
fn a_member_some_cut_recorded_is_grandfathered_into_include_and_advance() {
    // Given: a release cut from alpha and beta — the cut event records both as
    // parents — with no verdict behind either, as every member predating the
    // gate stands. Beta is then dropped.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(&lab, &home, &["drop", "feat/beta", "--why", "recut"]);
    assert!(dropped.status.success(), "{dropped:?}");

    // When: beta is included again, still with no verdict.
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);

    // Then: it is an existing member and comes back unasked.
    assert!(included.status.success(), "{included:?}");
    assert!(
        String::from_utf8_lossy(&included.stdout).contains("included feat/beta"),
        "{included:?}"
    );

    // And: a member that grows is advanced unasked — the repository ties its
    // tip to a current parent.
    lab::extend_branch(&lab, "feat/alpha", "alpha2.txt", "more alpha\n");
    let advanced = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    assert!(advanced.status.success(), "{advanced:?}");
    assert!(
        String::from_utf8_lossy(&advanced.stdout).contains("advanced feat/alpha"),
        "{advanced:?}"
    );
}
