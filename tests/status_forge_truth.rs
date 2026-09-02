//! Status facts the forge settles: a merged pull request the trunk contains, a
//! workflow the forge is holding for approval, a claim nothing names, and a
//! working copy that is only stale when jj itself would say so.

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

use std::collections::BTreeMap;

use knives::commands::status::{self, BranchState};
use knives::detect::{FindingKind, LandedVerdict};
use knives::forge::{CheckRun, ChecksSummary, MergeCommit, PullRequest};
use knives::ids::{BranchName, RepoName};
use knives::jj::Repo;
use knives::store::Store;
use lab::{Lab, commit_at, lab_entry};

fn gather(
    lab: &Lab,
    forge: &knives::forge::fake::FakeForge,
    store: &Store,
    probe: bool,
) -> status::Report {
    status::gather(
        &RepoName::new("demo"),
        &lab_entry(lab),
        store,
        &status::Options {
            probe,
            forge: Some(forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather")
}

fn row<'a>(report: &'a status::Report, name: &str) -> &'a status::BranchRow {
    report
        .branches
        .iter()
        .find(|row| row.name.as_str() == name)
        .unwrap_or_else(|| panic!("no row for {name}: {report:?}"))
}

#[test]
fn a_squash_merged_pull_the_trunk_contains_reads_in_trunk_despite_a_conflicting_replay() {
    // Given: a branch squash-merged with maintainer edits, so replaying it onto
    // the trunk conflicts with its own squash — the shape that read
    // `conflicts-with-trunk` for every merged pull request on a real fork.
    let lab = Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 7);
    lab.squash_merge_pull(7, Some("maintainer edit\n"));
    let landing = commit_at(&lab, "main@upstream").as_str().to_owned();
    let head = commit_at(&lab, "feature").as_str().to_owned();
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feature"),
            PullRequest {
                head_ref_oid: head,
                merge_commit: Some(MergeCommit { oid: landing }),
                ..pulls::pull_request(7, "MERGED", "feature")
            },
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    // When: status gathers with the landed probe on
    let report = gather(&lab, &forge, &store, true);

    // Then: the forge's landing commit settles what the replay could not
    let feature = row(&report, "feature");
    assert_eq!(feature.landed, Some(LandedVerdict::InTrunk), "{report:?}");
    assert_eq!(feature.state, BranchState::Landed);
}

#[test]
fn a_branch_carrying_work_past_its_merged_pull_keeps_the_replay_verdict_and_says_why() {
    // Given: the same squash-merge, then the branch grows a commit the pull never had
    let lab = Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 7);
    let merged_head = commit_at(&lab, "feature").as_str().to_owned();
    lab.squash_merge_pull(7, Some("maintainer edit\n"));
    let landing = commit_at(&lab, "main@upstream").as_str().to_owned();
    lab.jj_work(["new", "feature", "-m", "after the merge"]);
    std::fs::write(lab.work_path().join("later.txt"), "later\n").expect("write later");
    lab.jj_work(["bookmark", "set", "feature", "-r", "@"]);
    lab.jj_work(["new"]);
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feature"),
            PullRequest {
                head_ref_oid: merged_head,
                merge_commit: Some(MergeCommit { oid: landing }),
                ..pulls::pull_request(7, "MERGED", "feature")
            },
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    let report = gather(&lab, &forge, &store, true);

    // Then: the replay verdict stands, and a note names the reason
    let feature = row(&report, "feature");
    assert_ne!(feature.landed, Some(LandedVerdict::InTrunk), "{report:?}");
    assert!(
        report.notes.iter().any(|note| note.contains("#7 merged as")
            && note.contains("does not reach; landed is judged by replay")),
        "{report:?}"
    );
}

#[test]
fn a_workflow_awaiting_approval_is_action_required_not_ok() {
    // Given: an open pull request whose only completed check is the one that ran
    // unconditionally, with a gated workflow the forge refused to start
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let forge = knives::forge::fake::FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pulls::pull_request(11, "OPEN", "feat/alpha"),
        )]),
        checks: BTreeMap::from([(
            11,
            ChecksSummary {
                runs: vec![
                    CheckRun {
                        name: "lint-pr-title".to_owned(),
                        conclusion: Some("SUCCESS".to_owned()),
                    },
                    CheckRun {
                        name: "integration".to_owned(),
                        conclusion: Some("ACTION_REQUIRED".to_owned()),
                    },
                ],
            },
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    let report = gather(&lab, &forge, &store, false);

    // Then: the cell says what happened, and the finding names the held workflow
    let alpha = row(&report, "feat/alpha");
    assert_eq!(
        alpha.checks.as_deref(),
        Some("action-required"),
        "{report:?}"
    );
    let group = report
        .findings
        .iter()
        .find(|group| group.kind == FindingKind::ChecksFailing)
        .expect("a checks-failing finding");
    assert_eq!(group.subjects().collect::<Vec<_>>(), ["#11"]);
    let rendered = status::render::render(&report, true);
    assert!(
        rendered.contains(
            "1 check(s) held for action (an unapproved workflow runs nothing): integration"
        ),
        "{rendered}"
    );
}

#[test]
fn a_claim_on_a_branch_nothing_names_is_an_orphaned_claim_finding() {
    // Given: a claim whose bookmark is gone from every remote and whose
    // workspace was never opened, beside a claim on a live branch
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = RepoName::new("demo");
    let state = tempfile::tempdir().expect("state directory");
    let mut store =
        Store::open_for_update(state.path().join("state.json")).expect("open store for update");
    let identity = knives::commands::claim::Identity {
        owner: "ubuntu".to_owned(),
        kind: knives::store::OwnerKind::OsUser,
    };
    for branch in ["feat/alpha", "fix/deleted-long-ago"] {
        let target = knives::ids::BranchTarget::new(name.clone(), BranchName::new(branch));
        let _ = store.claim(&target, &identity, "started work");
    }
    store.save().expect("save store");
    let forge = knives::forge::fake::FakeForge::default();

    let report = gather(&lab, &forge, &store, false);

    // Then: only the claim nothing names is a finding; the live one is a row cell
    let orphaned = report
        .findings
        .iter()
        .find(|group| group.kind == FindingKind::OrphanedClaim)
        .expect("an orphaned-claim finding");
    assert_eq!(
        orphaned.subjects().collect::<Vec<_>>(),
        ["fix/deleted-long-ago"]
    );
    assert!(row(&report, "feat/alpha").claim.is_some());
}

#[test]
fn a_fetch_made_with_ignore_working_copy_does_not_make_the_working_copy_stale() {
    // Given: a checkout whose repository moved on through an operation that
    // touched no working copy — exactly what `knives sync` does
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.advance_upstream("upstream moved\n");
    let jj_ignoring_working_copy = |args: &[&str]| {
        let output = std::process::Command::new("jj")
            .arg("--repository")
            .arg(lab.work_path())
            .arg("--ignore-working-copy")
            .args(args)
            .output()
            .expect("run jj");
        assert!(output.status.success(), "{output:?}");
    };
    jj_ignoring_working_copy(&["git", "fetch", "--all-remotes"]);

    // Then: the working copy is not stale — jj would run there without complaint
    let repo = Repo::open(lab.work_path()).expect("open repo");
    assert_eq!(repo.stale_working_copy(lab.work_path()), None);

    // And: moving the working-copy commit to a different tree behind the
    // working copy's back — what `jj edit` from another handle does — IS stale,
    // exactly the case jj refuses to run in.
    jj_ignoring_working_copy(&["edit", "feat/alpha"]);
    let repo = Repo::open(lab.work_path()).expect("reopen repo");
    let stale = repo.stale_working_copy(lab.work_path());
    assert!(
        stale
            .as_deref()
            .is_some_and(|text| text.contains("working copy is stale")),
        "{stale:?}"
    );
}
