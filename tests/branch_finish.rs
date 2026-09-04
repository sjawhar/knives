//! `knives finish`, `track` and `depends`: the claim comes back and the ledger remembers why.
//!
//! Finish never consults the forge, refuses the primary workspace and another's
//! claim without `--force --why`, releases by possession or by owner when
//! checkout activity is unavailable, and records only what happened. Track and
//! depends leave both statements in the ledger; a fork-only statement is the
//! decision it is.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/forge_shim.rs"]
mod forge_shim;
#[path = "common/lab.rs"]
mod lab;

use forge_shim::{install_failing_gh, path_with_gh_shim};
use knives::ids::BranchName;
use knives::jj::Repo;
use knives::store::{OwnerKind, Store};
use lab::{Lab, release_test_home};
use serde_json::Value;
use std::process::Command;

#[test]
fn starting_and_finishing_a_branch_leaves_its_reason_in_the_ledger() {
    // Given: a managed fork and a config home
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary with a reason
    let started = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/alpha",
            "--repo",
            "demo",
            "--why",
            "carrying the queue fix",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run start");
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    // Then: the ledger holds the claim event. `start` opens a workspace at the
    // base revision and does not create a bookmark, so only the Scribe may decide
    // whether a ref anchor exists; here it correctly records none.
    let ledger = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"));
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].kind, knives::ledger::Kind::Event);
    assert_eq!(entries[0].owner, "ses_fff688");
    assert_eq!(entries[0].subject.as_deref(), Some("feat/alpha"));
    assert_eq!(entries[0].text, "claimed: carrying the queue fix");
    assert_eq!(entries[0].anchor, None);

    // When: it is handed back naming its successor
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/alpha",
            "--repo",
            "demo",
            "--superseded-by",
            "feat/replacement",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run finish");
    assert!(
        finished.status.success(),
        "finish failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );

    // Then: the supersession is recorded as an event rather than only as state
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(entries.len(), 2, "was: {entries:?}");
    assert_eq!(
        entries[1].text,
        "claim released; superseded by feat/replacement"
    );
}

/// A claim written straight into the store, so a `finish` test starts from a held
/// branch without `start` putting its own event in the ledger first.
fn hold_claim(home: &tempfile::TempDir, branch: &str) {
    let mut store = Store::open_for_update(home.path().join("state.json")).expect("open store");
    let _ = store.claim(
        &knives::ids::BranchTarget::new(
            knives::ids::RepoName::new("demo"),
            BranchName::new(branch),
        ),
        &knives::commands::claim::Identity {
            owner: "ses_fff688".to_owned(),
            kind: OwnerKind::HarnessSession,
        },
        "carrying the queue fix",
    );
    store.save().expect("save store");
}

fn start_claim_for_finish(
    lab: &lab::Lab,
    home: &tempfile::TempDir,
    owner: &str,
    branch: &str,
) -> std::path::PathBuf {
    let started = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            branch,
            "--repo",
            "demo",
            "--why",
            "carry the queue fix",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", owner)
        .output()
        .expect("start held branch");
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    lab.work
        .parent()
        .expect("workspace parent")
        .join(knives::commands::wip::workspace_for(branch))
}

fn knives_finish(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "finish"]);
    command.args(args);
    command
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run finish")
}

fn knives_finish_with_failing_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    args: &[&str],
    log: &std::path::Path,
) -> std::process::Output {
    let shim = tempfile::tempdir().expect("create failing forge shim directory");
    install_failing_gh(shim.path(), log);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "finish"]);
    command.args(args);
    command
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "ses_fff688")
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run finish with a failing forge shim")
}

#[test]
fn finish_releases_without_consulting_the_forge() {
    // Releasing a claim is a local act: the branch, its bookmark, and any open
    // pull request survive it, so finish has no question to ask the forge.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");
    {
        let target = knives::ids::BranchTarget::new(
            knives::ids::RepoName::new("demo"),
            BranchName::new("feat/alpha"),
        );
        let mut store = Store::open_for_update(home.path().join("state.json")).expect("open store");
        store.track_pull(&target, 7);
        store.save().expect("save stated pull");
    }
    let tip_before = lab.revision(&lab.work, "feat/alpha", "commit_id");
    let state = tempfile::tempdir().expect("test state");
    let log = state.path().join("gh.log");

    let finished = knives_finish_with_failing_forge(&lab, &home, &["feat/alpha"], &log);

    let stdout = String::from_utf8_lossy(&finished.stdout);
    assert!(
        finished.status.success(),
        "finish did not release the claim: {stdout}\n{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(stdout.contains("claim released"), "was: {stdout}");
    assert!(
        !log.exists(),
        "finish consulted the forge: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    assert!(
        state["claims"].get("demo/feat/alpha").is_none(),
        "claim remained: {}",
        state["claims"]
    );
    assert_eq!(
        state["tracked_pulls"]["demo/feat/alpha"],
        Value::from(7),
        "the stated pull request did not survive the release"
    );
    assert_eq!(
        lab.revision(&lab.work, "feat/alpha", "commit_id"),
        tip_before,
        "the branch did not survive the release untouched"
    );
}

#[test]
fn finish_refuses_a_branch_that_maps_to_the_primary_workspace() {
    // `start` can never have created a workspace named "default" — jj owns that
    // name — so a branch whose flattened name lands on it is a collision with
    // the checkout itself, not a workspace to forget and remove.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    let refused = knives_finish(&lab, &home, &["default"]);

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(
        refused.status.code(),
        Some(2),
        "stdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert!(
        stderr.contains("registered checkout itself"),
        "stderr: {stderr}"
    );
    assert!(lab.work.is_dir(), "the checkout was removed");
    assert!(
        !lab.revision(&lab.work, "@", "commit_id").is_empty(),
        "the primary workspace was forgotten"
    );
}

#[test]
fn finish_refuses_to_release_anothers_claim_without_force() {
    // A different harness session does not own the held workspace, so finishing
    // must leave both the claim and its workspace intact until force is explicit.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start_claim_for_finish(&lab, &home, "agent-one", "feat/gamma");

    let refused = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "finish", "feat/gamma", "--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "agent-two")
        .output()
        .expect("finish another agent's claim");

    assert_eq!(
        refused.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert!(
        stderr.contains("knives finish feat/gamma --force --why"),
        "stderr: {stderr}"
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-one".to_owned())
    );
    assert!(workspace.is_dir(), "foreign workspace was removed");
}

#[test]
fn finish_force_releases_and_records_provenance() {
    // A forced release needs an enduring, independently inspectable record of
    // whose claim it released, how it was identified, and why it was forced.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = start_claim_for_finish(&lab, &home, "agent-one", "feat/gamma");

    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--force",
            "--why",
            "owner session died",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "agent-two")
        .output()
        .expect("force finish another agent's claim");

    assert!(
        finished.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&finished.stdout),
        String::from_utf8_lossy(&finished.stderr)
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    assert!(
        state["claims"].get("demo/feat/gamma").is_none(),
        "claim remained: {}",
        state["claims"]
    );
    let events = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "agent-two")
        .output()
        .expect("read forced-release events");
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    let events = String::from_utf8_lossy(&events.stdout);
    assert!(
        events.contains("released agent-one's claim by force"),
        "events: {events}"
    );
    assert!(
        events.contains("(harness-session, claimed ") && events.contains(", last seen "),
        "events: {events}"
    );
    assert!(events.contains("owner session died"), "events: {events}");
}

#[test]
fn finish_force_requires_why() {
    // Clap must reject an unexplained forced release before it can mutate a claim.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--force",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("parse forced finish");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn finish_by_possession_still_releases() {
    // Physical presence in the claim's workspace is an intentional possession
    // proof even when no harness identity survives into the finishing process.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start_claim_for_finish(&lab, &home, "agent-one", "feat/gamma");

    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "finish", "feat/gamma", "--repo", "demo"])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env_remove("KNIVES_OWNER")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env("USER", "terminal-user")
        .output()
        .expect("finish held workspace by possession");

    assert!(
        finished.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&finished.stdout),
        String::from_utf8_lossy(&finished.stderr)
    );
}

#[test]
fn finishing_a_held_branch_without_a_successor_records_only_the_release() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");

    let finished = knives_finish(&lab, &home, &["feat/alpha"]);
    assert!(finished.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].text, "claim released");
}

#[test]
fn finishing_a_branch_nobody_held_records_no_release_that_never_happened() {
    // The ledger is the one record meant to be trusted months later, and an event
    // is a past-tense fact this tool observed. `finish` on an unheld branch
    // releases nothing — the command's own prose already says "was not held" —
    // so an entry claiming a release is a fabrication in the audit trail.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let finished = knives_finish(&lab, &home, &["feat/alpha"]);
    assert!(finished.status.success());
    assert!(
        String::from_utf8_lossy(&finished.stdout).contains("was not held"),
        "was: {}",
        String::from_utf8_lossy(&finished.stdout)
    );

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert!(
        entries.is_empty(),
        "a release that never happened: {entries:?}"
    );
}

#[test]
fn finishing_an_unheld_branch_still_records_the_supersession_it_did_record() {
    // Two acts, and either can happen alone: `--superseded-by` writes a
    // supersession into the store whether or not a claim was held, so the entry
    // says that and not the release that did not happen.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let finished = knives_finish(
        &lab,
        &home,
        &["feat/alpha", "--superseded-by", "feat/replacement"],
    );
    assert!(finished.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].text, "superseded by feat/replacement");
}

#[test]
fn stating_a_pull_request_and_a_dependency_leaves_both_statements_in_the_ledger() {
    // Given: a managed fork with a branch, and a sibling repo to depend on
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             [repos.sibling]\nupstream = \"https://forge.invalid/maintainer/other.git\"\n\
             origin = \"https://forge.invalid/acme/other.git\"\n",
            lab.upstream.display(),
        ),
    )
    .expect("write registry");
    let knives = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("JJ_CONFIG", "/dev/null")
            .env("KNIVES_OWNER", "ses_fff688")
            .output()
            .expect("run knives")
    };

    // When: the branch's pull request is stated, then a dependency, then the
    // statement is withdrawn
    assert!(
        knives(&["--text", "track", "feat/alpha", "--pr", "4545"])
            .status
            .success()
    );
    assert!(
        knives(&["--text", "depends", "feat/alpha", "--on", "sibling#49"])
            .status
            .success()
    );
    assert!(
        knives(&["--text", "track", "feat/alpha", "--forget"])
            .status
            .success()
    );

    // Then: all three statements are in order, anchored, and the stated pull
    // request is stamped on the entries written while it was stated
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    let texts: Vec<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "stated as #4545",
            "requires sibling#49",
            "pull request statement forgotten"
        ],
        "was: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .all(|entry| entry.subject.as_deref() == Some("feat/alpha"))
    );
    // Each entry is stamped with the number it is about: the one that created the
    // association, the one recorded while it stood, and the one it withdrew.
    assert_eq!(
        entries.iter().map(|entry| entry.pr).collect::<Vec<_>>(),
        [Some(4545), Some(4545), Some(4545)],
        "was: {entries:?}"
    );
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    assert!(
        entries
            .iter()
            .all(|entry| entry.anchor.as_deref() == Some(tip.as_str()))
    );

    // And: the whole chronology of that number is findable BY that number, which
    // is the only thing the stamped field is for. Stamping the pre-change value
    // on the statement event would have returned two of the three.
    let filtered = knives(&["--json", "notch", "--pr", "4545"]);
    let parsed: serde_json::Value =
        serde_json::from_slice(&filtered.stdout).expect("notch --json emits JSON");
    assert_eq!(parsed["matched"], 3, "was: {parsed}");
}

#[test]
fn a_fork_only_statement_is_recorded_as_the_decision_it_is() {
    let lab = lab::Lab::new();
    lab.branch("feat/ci-only", "ci.yml", "on: push\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "track",
            "feat/ci-only",
            "--fork-only",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run track");
    assert!(output.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "stated as having no upstream pull request");
}
