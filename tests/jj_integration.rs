#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::commands::{
    repos,
    status::{self, OriginRelation},
    sync,
};
use knives::config::RepoEntry;
use knives::detect::landed::RebaseOutcome;
use knives::forge::{ChecksSummary, Forge, ForgeError, PullRequest};
use knives::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName};
use knives::jj::{Repo, changed_files, changed_files_between, probe_landed, pull_heads};
use knives::store::Store;
use lab::Lab;
use std::collections::BTreeMap;
use std::process::Command;

struct StateUnavailableForge;

impl Forge for StateUnavailableForge {
    fn pull_requests(
        &self,
        _repo: &std::path::Path,
    ) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
        Ok(BTreeMap::new())
    }

    fn review_predates_head(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<bool>, ForgeError> {
        Ok(None)
    }

    fn checks(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<ChecksSummary>, ForgeError> {
        Ok(None)
    }

    fn pull_request_state(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Err(ForgeError::Command {
            command: "gh pr view".to_owned(),
            dir: "/repo".to_owned(),
            code: 1,
            stderr: "unavailable".to_owned(),
        })
    }

    fn newest_comment(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }
}

fn relation_to_origin(lab: &lab::Lab) -> Result<Option<OriginRelation>, knives::jj::JjError> {
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let origin_tip = repo
        .resolve_commit("feat/alpha@origin")
        .expect("origin tip");

    status::relation_to_origin(&repo, &tip, Some(&origin_tip))
}

/// Registry home + consumer for release-cut tests: one repo named `demo`,
/// one consumer following the current release by branch.
fn release_test_home(lab: &lab::Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", branch = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", branch = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write registry");
    (home, consumer)
}

#[test]
fn a_fork_whose_trunk_is_dev_probes_and_forks_against_dev() {
    // Given: an upstream whose only branch is dev, and a feature branch on it
    let lab = Lab::with_trunk("dev");
    lab.branch("feat/alpha", "feature.txt", "content\n");
    // When: the landed probe measures against dev@upstream
    let outcome = knives::jj::probe_landed(
        lab.work_path(),
        &knives::ids::BranchName::new("feat/alpha"),
        "dev@upstream",
    )
    .expect("probe runs");
    // Then: unmerged work replays clean and non-empty — the probe found the
    // trunk rather than erroring on a nonexistent main
    assert_eq!(outcome, RebaseOutcome::CleanNonEmpty);

    lab.publish_pull("feat/alpha", 1);
    lab.squash_merge_pull(1, None);
    let outcome = knives::jj::probe_landed(
        lab.work_path(),
        &knives::ids::BranchName::new("feat/alpha"),
        "dev@upstream",
    )
    .expect("probe runs after squash merge");
    assert_eq!(outcome, RebaseOutcome::Empty);
}

#[test]
fn a_forgotten_and_abandoned_release_disappears_and_the_remote_keeps_it() {
    // Given: a pushed release-shaped merge and a chained feature pair. Forget
    // alone leaves the remote-tracking ref pinning the release (abandon then
    // refuses "immutable"); forget --include-remotes releases the pin. The
    // chain requires multi-id abandon to act in one invocation.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    lab.jj_work(["new", "-r", "feat/alpha", "-m", "feat/alpha-child"]);
    std::fs::write(lab.work.join("alpha-child.txt"), "alpha child\n").expect("write child");
    lab.jj_work(["bookmark", "set", "feat/alpha-child", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let release = repo
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let alpha_child = repo
        .resolve_commit("feat/alpha-child")
        .expect("resolve alpha child");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");

    // When: the release is reaped in the load-bearing order.
    knives::jj::forget_bookmark_include_remotes(&lab.work, "release/2026-08-04").expect("forget");
    knives::jj::abandon_commits(&lab.work, std::slice::from_ref(&release)).expect("abandon");
    knives::jj::forget_bookmark_include_remotes(&lab.work, "feat/alpha").expect("forget alpha");
    knives::jj::forget_bookmark_include_remotes(&lab.work, "feat/alpha-child")
        .expect("forget alpha child");
    knives::jj::forget_bookmark_include_remotes(&lab.work, "feat/beta").expect("forget beta");
    knives::jj::abandon_commits(
        &lab.work,
        &[alpha.clone(), alpha_child.clone(), beta.clone()],
    )
    .expect("batch abandon");

    // Then: no ref of any kind remains and every abandoned commit is invisible.
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips.keys().any(|r| matches!(
            r.branch().as_str(),
            "release/2026-08-04" | "feat/alpha" | "feat/alpha-child" | "feat/beta"
        )),
        "reaped refs survived: {tips:?}"
    );
    // Visibility check, verified empirically: naming a hidden commit id in a
    // revset RESURRECTS it into the resolution (`all() & <id>` still returns
    // it after abandon), so the only honest assertion is listing all() and
    // checking absence.
    let visible = knives::jj::commits_matching(&lab.work, "all()").expect("query");
    assert!(
        !visible.contains(&release)
            && !visible.contains(&alpha)
            && !visible.contains(&alpha_child)
            && !visible.contains(&beta),
        "abandoned commits still visible: {visible:?}"
    );
    let orphans =
        knives::jj::commits_matching(&lab.work, "description(glob:\"feat/alpha-child*\")")
            .expect("query orphans");
    assert!(
        orphans.is_empty(),
        "a descendant survived: one-at-a-time abandon rewrote the later ids: {orphans:?}"
    );
    assert!(
        knives::jj::commits_matching(&lab.work, "none()")
            .expect("empty revset")
            .is_empty(),
        "none() returned commits"
    );
    // And: the remote still has the branch — reaping never touches the wire.
    let on_remote = std::process::Command::new("git")
        .args([
            "ls-remote",
            "--heads",
            lab.temp_origin().to_str().expect("utf-8"),
            "release/2026-08-04",
        ])
        .output()
        .expect("ls-remote");
    assert!(
        !String::from_utf8_lossy(&on_remote.stdout).trim().is_empty(),
        "remote branch was deleted"
    );
}

#[test]
fn reap_removes_superseded_cuts_and_keeps_the_newest() {
    // Given: two dated cuts, both pushed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: the older cut is gone in every form, the newest survives.
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04")
    );
    assert!(
        tips.keys()
            .any(|r| r.branch().as_str() == "release/2026-08-05")
    );
}

#[test]
fn reap_refuses_a_cut_that_has_local_descendants() {
    // Given: work stacked directly on a superseded cut — #4's third loss mode.
    // Reaping must never be the thing that drops it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked work"]);
    lab.jj_work(["new"]); // park the working copy elsewhere
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: nothing reaped; the reason names the descendant.
    assert!(report.reaped.is_empty(), "reaped: {:?}", report.reaped);
    assert_eq!(report.kept.len(), 1);
    assert!(report.kept[0].1.contains("descendant"), "{:?}", report.kept);
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04")
    );
}

#[test]
fn release_reap_returns_findings_when_a_superseded_cut_is_kept() {
    // Given: work stacked directly on a superseded cut, which reap must preserve.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked work"]);
    lab.jj_work(["new"]);
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the real reap command sees the protected older cut.
    let output = knives_release(&lab, &home, &["reap"]);

    // Then: the actionable kept result makes the command non-zero.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("demo: kept release/2026-08-04"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_reap_returns_findings_when_an_untracked_remote_pin_refuses_abandon() {
    // Given: an untracked remote pin still holds a superseded dated cut immutable.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let status = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("fetch superseded cut in second clone");
    assert!(status.success(), "fetch superseded cut in second clone");
    let status = Command::new("jj")
        .args([
            "bookmark",
            "create",
            "keep/pin",
            "-r",
            "release/2026-08-04@origin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create pin in second clone");
    assert!(status.success(), "create pin in second clone");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "keep/pin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();
    let (home, _consumer) = release_test_home(&lab);

    // When: standalone reap attempts to remove the superseded cut.
    let output = knives_release(&lab, &home, &["reap"]);

    // Then: the abandon refusal is printed and exits with Findings (1).
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("demo: ! release/2026-08-04: refs forgotten, abandon refused:"),
        "{stdout}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_second_workspaces_parked_working_copy_does_not_block_reaping() {
    // Given: another workspace parked (empty, undescribed) on the superseded
    // cut — knives' normal multi-workspace state. jj only auto-discards the
    // CURRENT workspace's @ when it moves; a second workspace's stays.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let parked_workspace = lab
        .work
        .parent()
        .expect("workspace has parent")
        .join("parked-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "parked",
        "--revision",
        "release/2026-08-04",
        parked_workspace.to_str().expect("utf-8 path"),
    ]);
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: the parked working copy does not block — the clause this pins.
    assert_eq!(
        report.reaped,
        vec!["release/2026-08-04".to_owned()],
        "{report:?}"
    );
}

#[test]
fn reap_reaps_when_another_remote_bookmark_pins_the_cut() {
    // Given: a non-dated origin bookmark created and pushed from work itself.
    // Fetch returns this pin as TRACKED, so it is the mutable contrast to the
    // untracked-pin sibling test.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.jj_work(["bookmark", "create", "keep/pin", "-r", "release/2026-08-04"]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "keep/pin",
    ]);
    lab.fetch_work();
    let tips = Repo::open(&lab.work)
        .expect("open fixture")
        .bookmark_tips()
        .expect("fixture tips");
    assert!(
        tips.keys().any(|reference| {
            matches!(
                reference,
                BookmarkRef::Remote { branch, remote }
                    if branch.as_str() == "keep/pin" && remote.as_str() == "origin"
            )
        }),
        "fixture expects keep/pin@origin"
    );

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: the tracked remote pin is mutable, so the dated cut is reaped.
    assert_eq!(
        report.reaped,
        vec!["release/2026-08-04".to_owned()],
        "{report:?}"
    );
    assert!(
        report.forgotten_only.is_empty() && report.notes.is_empty(),
        "{report:?}"
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("remaining tips");
    assert!(
        tips.keys().any(|reference| {
            matches!(
                reference,
                BookmarkRef::Remote { branch, remote }
                    if branch.as_str() == "keep/pin" && remote.as_str() == "origin"
            )
        }),
        "reaping must leave keep/pin@origin alone"
    );
}

#[test]
fn an_untracked_remote_pin_makes_abandon_refuse_and_lands_in_forgotten_only() {
    // Given: two cuts pushed; a pin bookmark on the superseded cut created in
    // ANOTHER clone and pushed from there, so it arrives in work UNTRACKED —
    // an immutable head (builtin_immutable_heads includes
    // untracked_remote_bookmarks()). The tracked-pin sibling test shows the
    // mutable contrast.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let status = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("fetch superseded cut in second clone");
    assert!(status.success(), "fetch superseded cut in second clone");
    let status = Command::new("jj")
        .args([
            "bookmark",
            "create",
            "keep/pin",
            "-r",
            "release/2026-08-04@origin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create pin in second clone");
    assert!(status.success(), "create pin in second clone");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "keep/pin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: refs are forgotten, abandon refuses, and reaped does not overstate.
    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(report.forgotten_only, vec!["release/2026-08-04".to_owned()]);
    assert!(
        report.notes.iter().any(|note| note.contains("immutable")),
        "{report:?}"
    );
}

#[test]
fn a_refused_first_name_does_not_stop_reaping_later_names() {
    // Given: TWO superseded dated cuts, where the alphabetically first is held
    // immutable by an untracked remote pin and the second is freely reapable.
    // The fleet cleanup of 2026-08-07 saw a reap stop at its first immutable
    // commit instead of carrying on — this pins the continuation.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-06", "feat/alpha", "feat/beta");
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "origin",
        "--bookmark",
        "release/2026-08-04",
    ]);
    let status = Command::new("jj")
        .args(["git", "fetch", "--remote", "origin"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("fetch superseded cut in second clone");
    assert!(status.success(), "fetch superseded cut in second clone");
    let status = Command::new("jj")
        .args([
            "bookmark",
            "create",
            "keep/pin",
            "-r",
            "release/2026-08-04@origin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create pin in second clone");
    assert!(status.success(), "create pin in second clone");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "keep/pin",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("reap");

    // Then: the pinned first name refuses without stopping the second.
    assert_eq!(report.forgotten_only, vec!["release/2026-08-04".to_owned()]);
    assert_eq!(report.reaped, vec!["release/2026-08-05".to_owned()]);
    assert!(
        report.notes.iter().any(|note| note.contains("immutable")),
        "{report:?}"
    );
}

#[test]
fn reap_clears_a_ref_the_next_fetch_rematerialized() {
    // Given: a reaped workspace whose next fetch resurrected the superseded ref
    // as untracked (jj keeps no memory of forgotten refs; spec evidence item 2).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        lab.jj_work(["git", "push", "--remote", "origin", "--bookmark", name]);
    }
    let repo = Repo::open(&lab.work).expect("open");
    knives::commands::release::reap_superseded(&lab.work, &repo).expect("first reap");
    lab.fetch_work();
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04"),
        "fixture expects the fetch to re-materialize the ref; it did not"
    );

    // When: reaped again (idempotence is the contract).
    let repo = Repo::open(&lab.work).expect("reopen for second reap");
    let report = knives::commands::release::reap_superseded(&lab.work, &repo).expect("second reap");

    // Then: gone again.
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);
    let tips = Repo::open(&lab.work)
        .expect("final open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|r| r.branch().as_str() == "release/2026-08-04")
    );
}

#[test]
fn local_commit_after_push_is_ahead_of_origin() {
    // Given: a branch already pushed to origin and one additional local commit.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["new", "-r", "feat/alpha", "-m", "local advance"]);
    std::fs::write(lab.work.join("local.txt"), "local\n").expect("write local commit");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: the origin relation is resolved from both tips.
    let relation = relation_to_origin(&lab);

    // Then: origin is the ancestor, so local is ahead.
    assert!(relation.is_ok());
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Ahead)
    );
}

#[test]
fn origin_commit_after_push_leaves_local_branch_behind() {
    // Given: a branch pushed to origin, then advanced from another clone.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "untrack", "feat/alpha", "--remote", "origin"]);
    lab.advance_origin_branch("feat/alpha", "origin advance\n");
    lab.fetch_work();

    // When: the local and fetched origin tips are compared.
    let relation = relation_to_origin(&lab);

    // Then: origin is the descendant, so local is behind.
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Behind)
    );
}

#[test]
fn rewritten_local_branch_is_diverged_from_origin() {
    // Given: a pushed branch whose local tip was rewritten without updating origin.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.rewrite_local_branch("feat/alpha", "rewritten\n");

    // When: the origin relation is resolved from mutually unreachable tips.
    let relation = relation_to_origin(&lab);

    // Then: neither side is announced as behind the other.
    assert!(relation.is_ok());
    assert_eq!(
        relation.expect("resolved relation"),
        Some(OriginRelation::Diverged)
    );
}

#[test]
fn an_unresolvable_origin_tip_returns_an_error() {
    // Given: a real local branch and an origin id the repository cannot resolve.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let branch = BranchName::new("feat/alpha");
    let tip = repo.resolve_commit(branch.as_str()).expect("local tip");
    let unresolved = CommitId::new("1111111111111111111111111111111111111111");

    // When: the resolver compares local history to that absent origin tip.
    let error = status::relation_to_origin(&repo, &tip, Some(&unresolved))
        .expect_err("an unresolved origin tip must not become a relation");

    // Then: the caller receives an error to report rather than a history verdict.
    assert!(error.to_string().contains(unresolved.as_str()));
}

#[test]
fn ancestry_is_answered_in_both_directions() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    let base = repo.resolve_commit("main").expect("main");

    assert!(repo.is_ancestor(&base, &tip).expect("base is behind tip"));
    assert!(
        !repo
            .is_ancestor(&tip, &base)
            .expect("tip is not behind base")
    );
}

#[test]
fn a_fixed_pin_locked_to_an_ancestor_is_behind() {
    let lab = Lab::new();
    lab.branch("integration", "base.txt", "base\n");
    let repo = Repo::open(&lab.work).expect("open ancestor");
    let ancestor = repo.resolve_commit("integration").expect("ancestor");
    lab.jj_work(["new", "-r", "integration", "-m", "advance integration"]);
    std::fs::write(lab.work.join("advance.txt"), "advance\n").expect("advance integration");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new"]);

    let consumer = tempfile::tempdir().expect("consumer directory");
    let locked: String = ancestor.as_str().chars().take(12).collect();
    std::fs::write(
        consumer.path().join("uv.lock"),
        format!("url = \"https://forge.invalid/o/repo.git?branch=integration#{locked}\"\n"),
    )
    .expect("consumer pin");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: "https://forge.invalid/up/repo.git".to_owned(),
        origin: "https://forge.invalid/o/repo.git".to_owned(),
        base: None,
        release: None,
        release_branch: Some("integration".to_owned()),
        test_count_command: None,
        consumers: vec![consumer.path().to_owned()],
    };
    let repo = Repo::open(&lab.work).expect("open advanced branch");

    let lag = repos::pin_lag(&entry, None, Some(&repo));

    assert!(
        lag.notes
            .iter()
            .any(|note| note.contains("pins read from the working copy")),
        "notes: {:?}",
        lag.notes
    );
    assert!(
        lag.lag.as_ref().is_some_and(|lag| lag.contains(&locked)),
        "lag: {:?}",
        lag.lag
    );
}

#[test]
fn a_consumer_checkout_parked_behind_its_origin_does_not_produce_a_false_behind() {
    // Given: a consumer repo whose origin trunk pins the newest release while
    // the checkout's working copy still shows an older pin — the exact state
    // that produced false BEHIND findings twice.
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );

    // When: the consumer is scanned.
    let (pins, notes) = knives::commands::release::scan_consumer_for(
        &consumer,
        Some("tool"),
        &knives::ids::ReleaseScheme::Dated,
    );

    // Then: the pin is the origin trunk's, and the checkout's lag is a note.
    assert_eq!(pins.len(), 1, "was: {pins:?}");
    assert_eq!(pins[0].reference, "release/2026-07-28");
    assert!(
        notes.iter().any(|note| note.contains("behind")),
        "the stale checkout is annotated, not silently trusted: {notes:?}"
    );
}

#[test]
fn a_dev_trunk_consumer_checkout_uses_its_origin_head_pin() {
    let lab = Lab::with_trunk("dev");
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );

    let (pins, notes) = knives::commands::release::scan_consumer_for(
        &consumer,
        Some("tool"),
        &knives::ids::ReleaseScheme::Dated,
    );

    assert_eq!(pins.len(), 1, "was: {pins:?}");
    assert_eq!(pins[0].reference, "release/2026-07-28");
    assert!(
        notes.iter().any(|note| note.contains("origin/dev")),
        "the origin default branch is preserved: {notes:?}"
    );
}

#[test]
fn a_consumer_without_an_origin_remote_uses_its_current_working_copy_pin() {
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );
    lab.reset_consumer_to_origin(&consumer);
    lab.rename_consumer_remote(&consumer, "origin", "upstream");
    let entry = RepoEntry {
        path: lab.work,
        upstream: "https://forge.invalid/up/tool.git".to_owned(),
        origin: "https://forge.invalid/o/tool.git".to_owned(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: vec![consumer.clone()],
    };

    let pin_lag = repos::pin_lag(&entry, Some(&"release/2026-07-28@origin".to_owned()), None);

    assert_eq!(pin_lag.lag, None, "was: {pin_lag:?}");
    assert_eq!(
        pin_lag.notes,
        vec![format!(
            "{}: no origin trunk resolved; pins read from the working copy",
            consumer.display()
        )]
    );
}

#[test]
fn a_tip_carried_into_another_branch_is_found() {
    // Given: a maintainer branch built on our branch's tip
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    // When: bookmarks carrying the original tip are listed
    let repo = Repo::open(&lab.work).expect("reopen");
    let carriers = repo
        .branches_containing(&tip, &ReleaseScheme::Dated)
        .expect("carriers");
    let named: Vec<String> = carriers.iter().map(ToString::to_string).collect();

    // Then: the other branch is included and the branch itself is not
    assert!(
        named.iter().any(|name| name.contains("theirs/rework")),
        "was: {named:?}"
    );
    assert!(
        !named.iter().any(|name| name == "feat/alpha"),
        "a branch does not carry itself: {named:?}"
    );
}

#[test]
fn a_release_cut_is_not_a_carrier_locally_or_at_origin() {
    // Given: a flat release cut that carries our feature branch, then is pushed.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.octopus("release/2026-07-30", "feat/alpha", "feat/beta");
    lab.push_branch("release/2026-07-30");

    // When: carriers of the feature tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated)
        .expect("carriers");

    // Then: the release is not reported through either representation we own.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("release/2026-07-30"))),
        "local release was reported: {carriers:?}"
    );
    assert!(
        !carriers.contains(&BookmarkRef::Remote {
            branch: BranchName::new("release/2026-07-30"),
            remote: RemoteName::new("origin"),
        }),
        "origin release was reported: {carriers:?}"
    );
}

#[test]
fn git_tracking_refs_are_not_carriers_but_other_branches_are() {
    // Given: a maintainer branch carrying our tip and jj's matching git-tracking ref.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    // When: carriers of our tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated)
        .expect("carriers");

    // Then: the real branch remains useful evidence, but jj's duplicate does not.
    assert!(
        carriers.contains(&BookmarkRef::Local(BranchName::new("theirs/rework"))),
        "the ordinary carrier was lost: {carriers:?}"
    );
    assert!(
        !carriers.iter().any(|reference| {
            matches!(reference, BookmarkRef::Remote { remote, .. } if remote.as_str() == "git")
        }),
        "git-tracking refs were reported: {carriers:?}"
    );
}

#[test]
fn fetched_pull_request_heads_are_not_carriers() {
    // Given: a fetched pull-request head that descends from our branch tip.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    lab.jj_work(["bookmark", "create", "pr-4545", "-r", "feat/alpha"]);
    lab.jj_work(["new", "pr-4545", "-m", "fetched pull head advance"]);
    std::fs::write(lab.work.join("pull-head.txt"), "fetched\n").expect("write pull head");
    lab.jj_work(["bookmark", "set", "pr-4545", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: carriers of the feature tip are listed.
    let carriers = Repo::open(&lab.work)
        .expect("reopen")
        .branches_containing(&tip, &ReleaseScheme::Dated)
        .expect("carriers");

    // Then: our fetched pull request is not mistaken for someone else's carrier.
    assert!(
        !carriers.contains(&BookmarkRef::Local(BranchName::new("pr-4545"))),
        "fetched pull head was reported: {carriers:?}"
    );
}

#[test]
fn unavailable_state_for_a_tracked_pull_request_is_incomplete() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 42);
    let name = knives::ids::RepoName::new("a-repo");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let mut store = Store::open_for_update(lab.work.join("state.json")).expect("store");
    store.record_pull_head(&name, 42, "previous");

    let report = sync::sync_repo(&name, &entry, &mut store, Some(&StateUnavailableForge))
        .expect("sync report");

    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("state of #42 unavailable")),
        "was: {report:?}"
    );
    assert!(report.notes.is_empty(), "was: {report:?}");
    assert_eq!(sync::exit_for(&report), knives::cli::Exit::Incomplete);
}

#[test]
fn bookmark_tips_keeps_local_and_remote_refs_distinct() {
    // Given: a fork checkout with the same branch name locally and on origin.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.rewrite_local_branch("feature", "rewritten\n");

    // When: typed bookmark tips are read through jj-lib.
    let tips = Repo::open(&lab.work)
        .expect("open repository")
        .bookmark_tips()
        .expect("read tips");

    // Then: the local and remote references remain separate map keys.
    let local = BookmarkRef::Local(BranchName::new("feature"));
    let remote = BookmarkRef::Remote {
        branch: BranchName::new("feature"),
        remote: RemoteName::new("origin"),
    };
    assert_ne!(tips.get(&local), tips.get(&remote));
}

#[test]
fn parents_of_octopus_includes_bookmarks_for_every_parent() {
    // Given: an octopus merge over two labelled branches and main.
    let lab = lab::Lab::new();
    lab.branch("one", "one.txt", "one\n");
    lab.branch("two", "two.txt", "two\n");
    lab.octopus("release", "one", "two");

    // When: its parents are read through jj-lib.
    let parents = Repo::open(&lab.work)
        .expect("open repository")
        .parents_of("release")
        .expect("read parents");

    // Then: every octopus parent retains its bookmark reference.
    assert_eq!(parents.len(), 3);
    assert!(parents.iter().any(|parent| {
        parent
            .bookmarks
            .contains(&BookmarkRef::Local(BranchName::new("one")))
    }));
    assert!(parents.iter().any(|parent| {
        parent
            .bookmarks
            .contains(&BookmarkRef::Local(BranchName::new("two")))
    }));
}

#[test]
fn squash_merge_lands_content_that_ancestry_cannot_see() {
    // The reason this crate probes instead of asking about ancestry. Both halves
    // are asserted, and the second goes through the crate, so a regression in
    // `probe_landed` fails this test rather than only the environment fact.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    lab.fetch_work();

    // Ancestry says the branch is not in the trunk.
    let merged_in = lab.revision(
        &lab.work,
        "feat/alpha & ::main@upstream",
        "commit_id ++ \"\\n\"",
    );
    assert!(
        merged_in.trim().is_empty(),
        "the branch became an ancestor; the premise is gone"
    );

    // The crate says its content is there anyway.
    let verdict = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/alpha"),
        "main@upstream",
    )
    .expect("probe");
    assert_eq!(verdict, knives::detect::RebaseOutcome::Empty);
}

#[test]
fn probe_landed_is_empty_after_plain_squash_merge() {
    // Given: a branch squash-merged unchanged upstream.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 8);
    lab.squash_merge_pull(8, None);

    // When: the branch is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: jj reports that replay as empty.
    assert_eq!(outcome, RebaseOutcome::Empty);
}

#[test]
fn probe_landed_is_conflicted_after_maintainer_changes_squash_content() {
    // Given: a branch squash-merged with maintainer edits.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 9);
    lab.squash_merge_pull(9, Some("maintainer rewrite\n"));

    // When: the branch is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: jj preserves the maintainer conflict.
    assert_eq!(outcome, RebaseOutcome::Conflicted);
}

#[test]
fn probe_landed_is_clean_nonempty_for_open_branch() {
    // Given: an open branch not present upstream.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.advance_upstream("advance\n");
    lab.rebase_and_force_push("feature");

    // When: it is replayed onto upstream main.
    let outcome = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");

    // Then: the replay is clean but still carries work.
    assert_eq!(outcome, RebaseOutcome::CleanNonEmpty);
}

#[test]
fn probe_landed_cleans_only_its_temporary_commits() {
    // Given: an open branch and a stable working copy.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    let children_before = lab.revision(&lab.work, "children(main@upstream)", "commit_id");
    let branch_before = lab.revision(&lab.work, "feature", "commit_id");
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");

    // When: landing is probed.
    probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream").expect("probe landed");

    // Then: only temporary probe commits have disappeared.
    assert_eq!(
        lab.revision(&lab.work, "children(main@upstream)", "commit_id"),
        children_before
    );
    assert_eq!(
        lab.revision(&lab.work, "feature", "commit_id"),
        branch_before
    );
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

/// Operation ids in the shared op log, newest first.
fn operation_ids(repo: &std::path::Path) -> Vec<String> {
    let output = Command::new("jj")
        .args([
            "--ignore-working-copy",
            "op",
            "log",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ])
        .current_dir(repo)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("read op log");
    assert!(output.status.success(), "read op log");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn probes_write_nothing_to_the_shared_op_log() {
    // Given: an open branch and upstream drift, so both probes do real replay
    // work. A probe answers a read-only question; in a repo shared by several
    // agents every operation it writes is a reconciliation point and op-log
    // noise (the shape that derailed the 2026-08-08 cut diagnosis).
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.advance_upstream("advance\n");
    let trunk = lab.revision(&lab.work, "main@upstream", "commit_id");
    let tip = lab.revision(&lab.work, "feature", "commit_id");
    let operations_before = operation_ids(&lab.work);

    // When: the landed and net-diff probes both run.
    let landed = probe_landed(&lab.work, &BranchName::new("feature"), "main@upstream")
        .expect("probe landed");
    let net = knives::jj::probe_net_diff(&lab.work, &trunk, &tip, &trunk).expect("probe net diff");

    // Then: real answers, and the op log gained nothing at all.
    assert_eq!(landed, RebaseOutcome::CleanNonEmpty);
    assert_eq!(net, RebaseOutcome::CleanNonEmpty);
    assert_eq!(operation_ids(&lab.work), operations_before);
}

#[test]
fn divergent_changes_reports_both_rewrites_after_fetch() {
    // Given: the same branch rewritten independently in two jj clones.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feature");

    // When: divergence is read through jj-lib after fetching.
    let divergent = Repo::open(&lab.work)
        .expect("open repository")
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read divergence");

    // Then: exactly one change has two visible commits.
    assert_eq!(divergent.len(), 2);
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
}

#[test]
fn divergent_changes_reports_copies_buried_under_descendants() {
    // Given: one change as two visible commits, EACH buried under a child, so
    // neither copy is a view head. This is the fleet's dominant shape — a
    // branch advanced past its rewritten ancestor while a remote-pinned chain
    // kept the old copy — and enumerating only head copies missed it (the jj
    // fork carried 74 divergent changes; 35 were reported).
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feature");
    // Bury the local rewrite: make the parked child real so it survives, then
    // bury the fetched rewrite under a child of its own.
    std::fs::write(lab.work.join("local-child.txt"), "local\n").expect("write local child");
    lab.jj_work(["describe", "-m", "child of local rewrite"]);
    lab.jj_work(["new", "feature@origin", "-m", "child of fetched rewrite"]);
    std::fs::write(lab.work.join("fetched-child.txt"), "fetched\n").expect("write fetched child");
    lab.jj_work(["status"]);

    // When: divergence is read through jj-lib.
    let divergent = Repo::open(&lab.work)
        .expect("open repository")
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read divergence");

    // Then: the buried pair is still reported.
    assert_eq!(divergent.len(), 2, "{divergent:?}");
    assert_eq!(divergent[0].0, divergent[1].0);
    assert_ne!(divergent[0].1, divergent[1].1);
}

#[test]
fn divergence_pinned_only_by_a_superseded_release_ref_is_not_reported() {
    // Given: one change as two commits, where the old copy's only visibility is a
    // remote-tracking ref we are told to ignore. This is the re-materialized
    // superseded-cut shape: bare `jj git fetch` brings such refs back forever,
    // so the reader must ignore them rather than the graph staying clean.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    let repo = Repo::open(&lab.work).expect("open");

    // Sanity: unfiltered, the divergence is visible (two commits, one change).
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("divergent unfiltered");
    assert!(
        !unfiltered.is_empty(),
        "fixture failed to create divergence"
    );

    // When: the ref holding the stale copy is ignored.
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("feat/alpha"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("divergent filtered");

    // Then: the finding is gone — the stale copy was visible only through the
    // ignored ref, so nothing else vouches for it.
    assert!(filtered.is_empty(), "still reported: {filtered:?}");
}

#[test]
fn divergence_with_only_ignored_head_keeps_copies_vouched_by_live_heads() {
    // Given: two rewrites of one change, each kept as a non-head ancestor of a
    // live head, plus a third rewrite whose sole head ref is a dated release.
    let lab = Lab::new();
    lab.branch("feat/alpha", "feature.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");

    std::fs::write(lab.work.join("keep-one.txt"), "keep\n").expect("write first child");
    lab.jj_work(["describe", "-m", "keep first rewrite"]);
    lab.jj_work(["new", "feat/alpha@origin", "-m", "keep second rewrite"]);
    std::fs::write(lab.work.join("keep-two.txt"), "keep\n").expect("write second child");
    lab.jj_work(["status"]);

    let status = Command::new("jj")
        .args(["edit", "--ignore-immutable", "feat/alpha"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("rewrite release head");
    assert!(status.success(), "rewrite release head");
    std::fs::write(lab.second.join("feature.txt"), "third rewrite\n").expect("write third rewrite");
    let status = Command::new("jj")
        .args(["bookmark", "create", "release/2024-01-01", "-r", "@"])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("create release bookmark");
    assert!(status.success(), "create release bookmark");
    let status = Command::new("jj")
        .args([
            "git",
            "push",
            "--remote",
            "origin",
            "--bookmark",
            "release/2024-01-01",
        ])
        .current_dir(&lab.second)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push release bookmark");
    assert!(status.success(), "push release bookmark");
    lab.fetch_work();
    lab.jj_work(["bookmark", "forget", "--include-remotes", "feat/alpha"]);
    let repo = Repo::open(&lab.work).expect("open");

    // When: the release ref is ignored.
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read unfiltered divergence");
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("release/2024-01-01"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("read filtered divergence");

    // Then: the ignored head is excluded but its two live-vouched sibling copies remain.
    assert_eq!(unfiltered.len(), 3, "fixture should expose three copies");
    assert_eq!(
        filtered.len(),
        2,
        "live-vouched copies disappeared: {filtered:?}"
    );
    assert_eq!(filtered[0].0, filtered[1].0);
}

#[test]
fn unrelated_divergence_survives_a_nonempty_ignored_ref_set() {
    // Given: independently rewritten alpha and beta branches.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    lab.rewrite_in_both_clones("feat/alpha");
    lab.branch("feat/beta", "beta.txt", "one\n");
    lab.rewrite_in_both_clones("feat/beta");
    let repo = Repo::open(&lab.work).expect("open");

    // When: only alpha's remote ref is ignored.
    let unfiltered = repo
        .divergent_changes(&std::collections::BTreeSet::new())
        .expect("read unfiltered divergence");
    let ignored = std::collections::BTreeSet::from([BookmarkRef::Remote {
        branch: BranchName::new("feat/alpha"),
        remote: RemoteName::new("origin"),
    }]);
    let filtered = repo
        .divergent_changes(&ignored)
        .expect("read filtered divergence");

    // Then: alpha is suppressed while beta's pair remains reported.
    assert_eq!(unfiltered.len(), 4, "fixture should expose two divergences");
    assert_eq!(
        filtered.len(),
        2,
        "beta divergence disappeared: {filtered:?}"
    );
    assert_eq!(filtered[0].0, filtered[1].0);
}

#[test]
fn pull_heads_reads_local_upstream_pull_refs() {
    // Given: a branch published at an upstream pull-ref namespace.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    lab.push_branch("feature");
    lab.publish_pull("feature", 42);

    // When: pull heads are listed through the git transport.
    let heads = pull_heads(
        &lab.work,
        lab.upstream.to_str().expect("utf-8 upstream path"),
    )
    .expect("read pull heads");

    // Then: the pull number maps to its published object id.
    assert!(heads.contains_key(&42));
}

#[test]
fn changed_files_reports_sorted_paths_without_snapshotting_working_copy() {
    // Given: a branch with one changed file and an untouched working copy.
    let lab = lab::Lab::new();
    lab.branch("feature", "feature.txt", "original\n");
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");

    // When: changed paths are requested for the branch revision.
    let files = changed_files(&lab.work, "feature").expect("read changed files");

    // Then: paths are normalized, sorted, and the working copy stays untouched.
    assert_eq!(files, vec!["feature.txt"]);
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

#[test]
fn changed_files_between_handles_a_branch_behind_advanced_upstream() {
    // Given: a branch whose upstream trunk has advanced since the branch forked.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "main@upstream",
        "-m",
        "merge upstream into branch",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let working_copy_before = lab.revision(&lab.work, "@", "change_id");
    let from = "fork_point(main@upstream | feat/alpha)";

    // When: the branch tree is compared directly with its fork point.
    let files = changed_files_between(&lab.work, from, "feat/alpha").expect("diff branch trees");

    // Then: the branch file is returned without changing the working copy.
    assert_eq!(files, vec!["alpha.txt"]);
    assert_eq!(
        lab.revision(&lab.work, "@", "change_id"),
        working_copy_before
    );
}

#[test]
fn the_net_probe_cleans_up_its_bookmark_and_commits() {
    // Given: a multi-commit member range, which requires a bookmark-tracked synthetic probe.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "first\n");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "first\nsecond\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let before_commits = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    let before_bookmarks = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--all-remotes",
        ])
        .output()
        .expect("list bookmarks before probe");
    assert!(
        before_bookmarks.status.success(),
        "bookmark list failed: {}",
        String::from_utf8_lossy(&before_bookmarks.stderr)
    );

    // When: the net probe creates, rewrites, and cleans up its synthetic commit.
    knives::jj::probe_net_diff(&lab.work, "main@origin", "feat/alpha", "main@origin")
        .expect("probe net diff");

    // Then: both globally visible commits and bookmarks exactly match their prior state.
    let after_commits = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    let after_bookmarks = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--all-remotes",
        ])
        .output()
        .expect("list bookmarks after probe");
    assert!(
        after_bookmarks.status.success(),
        "bookmark list failed: {}",
        String::from_utf8_lossy(&after_bookmarks.stderr)
    );
    assert_eq!(
        before_commits, after_commits,
        "the net probe left commits behind"
    );
    assert_eq!(
        before_bookmarks.stdout, after_bookmarks.stdout,
        "the net probe left bookmarks behind"
    );
}

#[test]
fn the_range_probe_cleans_up_every_scratch_commit() {
    // Given: an octopus range whose duplicate creates a parent/child scratch chain.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("feat/pair", "feat/alpha", "feat/beta");
    let before = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");

    // When: the landed-range probe cleans up the commits it duplicated.
    let outcome = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/pair"),
        "main",
    );
    assert!(
        outcome.is_ok(),
        "an octopus range must be probed, not refused: {outcome:?}"
    );

    // Then: enumerating every visible commit finds no scratch-chain residue.
    let after = lab.revision(&lab.work, "all()", "commit_id ++ \"\\n\"");
    assert_eq!(before, after, "the range probe left commits behind");
}

#[test]
fn a_new_workspace_is_based_on_the_upstream_trunk_not_the_current_change() {
    // The accident this default exists to prevent: an agent sitting in a release
    // workspace runs `jj new` and silently inherits the release merge as a
    // parent, so unrelated work rides into its pull request.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/dated", "feat/alpha", "feat/beta");
    // Move the upstream trunk so it differs from our fork's copy. Without this
    // the two are the same commit and the test passes whichever trunk the code
    // uses, proving nothing.
    lab.advance_upstream("moved on\n");

    // Given: the working copy is parked on the release merge, the dangerous spot
    let parked = lab.revision(&lab.work, "@", "change_id.short(8)");
    assert!(!parked.trim().is_empty());

    // When: a workspace is opened the way `knives start` opens one
    let destination = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &destination, "main@upstream")
        .expect("add workspace");

    // Then: its only parent is the upstream trunk, not the release merge
    let parents = lab.revision(&destination, "parents(@)", "commit_id.short(12) ++ \"\\n\"");
    let upstream = lab.revision(&lab.work, "main@upstream", "commit_id.short(12)");
    let listed: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(
        listed,
        vec![upstream.trim()],
        "new work must sit on the upstream trunk alone"
    );
}

#[test]
fn start_bases_a_new_branch_on_the_shared_base_not_the_advanced_upstream() {
    // Given: a release whose members fork from today's trunk, then upstream advances.
    // Basing new work on the advanced tip would drag that advance into the next
    // cut through one member — the mixed-base conflict storm (#10).
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let repo = Repo::open(&lab.work).expect("open");
    let base_before_advance = repo.resolve_commit("main@origin").expect("resolve base");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "test",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the shared base, not the advanced tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        base_before_advance.as_str(),
        "based on {parent}, expected the shared base"
    );
}

#[test]
fn start_without_a_release_uses_the_fetched_upstream_trunk() {
    // Given: a registry with no release and an upstream tip distinct from origin.
    let lab = lab::Lab::new();
    lab.advance_upstream("upstream advance\n");
    let upstream_trunk = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@upstream")
        .expect("resolve upstream trunk");
    let (home, _consumer) = release_test_home(&lab);

    // When: the binary starts a branch without a release base to select.
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/no-release",
            "--repo",
            "demo",
            "--why",
            "test",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: its parent is the fetched upstream trunk and the fallback is disclosed.
    let workspace = lab.work.parent().expect("parent").join("feat-no-release");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(parent, upstream_trunk.as_str());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("(the fetched upstream trunk)"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn shared_base_selects_the_newest_of_multiple_trunk_reachable_release_parents() {
    // Given: a release carrying an old origin trunk parent, a newer upstream trunk
    // parent, and a feature parent. This is the accumulated-bases shape #11 leaves
    // behind after upstream advances.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-m",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    let repo = Repo::open(&lab.work).expect("open");
    let release = repo
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let newest_trunk_parent = repo
        .resolve_commit("main@upstream")
        .expect("resolve upstream trunk");

    // When: the release's shared base is selected.
    let shared_base = knives::commands::release::shared_base(&repo, &release, &newest_trunk_parent)
        .expect("select shared base");

    // Then: the newer trunk parent wins, not the older accumulation residue.
    assert_eq!(shared_base, Some(newest_trunk_parent));
}

#[test]
fn a_flat_releases_shared_base_is_the_members_fork_point() {
    // Given: a doctrine-flat release — members only, the base never a parent —
    // and an upstream that has advanced past the members' fork point.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "flat release",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let fork_point = repo
        .resolve_commit("main@origin")
        .expect("resolve fork point");
    lab.advance_upstream("upstream advance\n");
    let reopened = Repo::open(&lab.work).expect("reopen");
    let release = reopened
        .resolve_commit("release/2026-08-04")
        .expect("resolve release");
    let trunk_tip = reopened
        .resolve_commit("main@upstream")
        .expect("resolve trunk tip");

    // When: the release's shared base is selected.
    let shared_base = knives::commands::release::shared_base(&reopened, &release, &trunk_tip)
        .expect("select shared base");

    // Then: it is the commit every member forks from, not the advanced tip and
    // not nothing — a flat release still has exactly one fork point.
    assert_eq!(shared_base, Some(fork_point));
}

#[test]
fn a_flat_release_recuts_after_upstream_drift_collides_with_a_member() {
    // Given: a flat two-member cut, then upstream lands a squash that rewrites
    // the same file alpha creates. Auditing against the trunk tip instead of
    // the fork point made innocent beta read as diverging: its synthetic diff
    // deletes the drifted file while the cut modifies it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.publish_pull("feat/gamma", 9);
    lab.squash_merge_pull(9, Some("upstream drift\n"));

    // When: the identical composition is re-cut under the new name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut lands — each member's content is measured from the fork
    // point, so upstream drift is not charged to the members. (The overall exit
    // still reports the unrelated lagging-trunk finding; only the audit is under
    // test here.)
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "audit false positive: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-05"),
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "the re-cut must carry the composition verbatim"
    );
}

#[test]
fn a_recut_carries_a_recorded_resolution_that_dropped_content() {
    // Given: two members conflicting on one file, the conflict resolved by hand
    // ON the release with content that matches neither side — a published
    // judgment that deliberately diverges from both members.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("cut release/2026-08-04 as"),
        "first cut was refused: {first:?}"
    );
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "merged\n").expect("resolve by hand");
    lab.jj_work(["new"]);

    // When: the identical composition is re-cut under a new name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the recorded resolution is carried, reported, and not refused —
    // the audit charges a cut only with divergence it introduces.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as"),
        "re-cut was refused: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("missing or diverges"),
        "a recorded resolution was refused as a loss: {stdout}"
    );
    assert!(
        stdout.contains(
            "feat/alpha: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        ) && stdout.contains(
            "feat/beta: diverges where the previous release already did \
             (a recorded resolution); carried forward"
        ),
        "missing carried-resolution report: {stdout}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-05", "shared.txt"),
        "merged\n",
        "the resolution must survive the re-cut"
    );
}

#[test]
fn a_first_cut_audits_members_from_their_fork_point_not_the_trunk_tip() {
    // Given: three branches forked from the seed, one of them squash-merged
    // upstream with maintainer edits that rewrite alpha's file. The first cut
    // has no previous composition, so its audit base is chosen from scratch.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "feature.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/gamma", 9);
    lab.squash_merge_pull(9, Some("upstream drift\n"));

    // When: the first cut merges every branch.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: it succeeds, flat, with exactly the three branches as parents.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "first cut failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), 3, "parents: {parents:?}");
    assert!(
        !parents.contains(&commit_at(&lab, "main@upstream")),
        "the trunk must not be a parent"
    );
}

#[test]
fn start_bases_a_new_branch_on_a_flat_releases_fork_point() {
    // Given: a doctrine-flat release — no trunk parent to find — and an
    // upstream that has advanced past the members' fork point.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work([
        "new",
        "-r",
        "feat/alpha",
        "-r",
        "feat/beta",
        "-m",
        "flat release",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let fork_point = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@origin")
        .expect("resolve fork point");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary.
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "test",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the workspace's @ sits on the members' fork point, not the tip.
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let parent = lab.revision(&workspace, "@-", "commit_id");
    assert_eq!(
        parent,
        fork_point.as_str(),
        "based on {parent}, expected the flat release's fork point"
    );
}

#[test]
fn release_plan_exits_with_findings_when_the_current_release_lags_the_upstream_trunk() {
    // Given: a clean dated release that was cut before upstream advanced.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_upstream("upstream advance\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release plan reports its warnings.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout);

    // Then: scripts receive the findings exit code for the actionable trunk warning.
    assert!(
        text.contains("does not contain the upstream trunk"),
        "trunk lag not rendered: {text}"
    );
    assert_eq!(output.status.code(), Some(1), "stdout: {text}");
}

#[test]
fn the_base_parent_is_not_stale_and_a_drifted_member_is_a_mixed_base_finding() {
    // Given: a release whose first parent is the bookmarkless shared base, and
    // one member re-based past it onto the advanced upstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_origin_branch("main", "origin advance\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    lab.advance_upstream("upstream advance\n");
    lab.rebase_and_force_push("feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: the bookmarkless base is not reported as a stale parent.
    assert!(
        !text.contains("carries no bookmark"),
        "base parent misread as stale: {text}"
    );
    // And: the drifted member is named as a mixed base.
    assert!(
        text.contains("feat/beta") && text.contains("beyond the shared base"),
        "mixed base not reported: {text}"
    );
    // And: the member still on the base is not reported.
    assert!(
        !text.contains("feat/alpha carries"),
        "well-based member misreported: {text}"
    );
    assert_eq!(output.status.code(), Some(1), "stdout: {text}");
}

#[test]
fn older_upstream_release_parent_is_reported_as_a_superseded_base() {
    // Given: a release that accumulated the old and newer upstream-trunk positions.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let base0 = lab.revision(&lab.work, "main@upstream", "commit_id");
    lab.advance_upstream("upstream advance\n");
    let base1 = lab.revision(&lab.work, "main@upstream", "commit_id");
    lab.jj_work([
        "new",
        "-r",
        base0.trim(),
        "-r",
        base1.trim(),
        "-r",
        "feat/alpha",
        "-m",
        "release/2026-08-04",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: the release plan is rendered.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout);

    // Then: the obsolete trunk parent is explicitly classified for repair.
    assert!(
        text.contains("older upstream base superseded by"),
        "superseded base not reported: {text}"
    );
}

#[test]
fn preflight_renders_a_mixed_base_finding_and_exits_with_findings() {
    // Given: a release with a bookmarkless base and a member rebased past it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.advance_origin_branch("main", "origin advance\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    lab.advance_upstream("upstream advance\n");
    lab.rebase_and_force_push("feat/beta");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let state = tempfile::tempdir().expect("create state directory");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("open store");

    // When: preflight gathers and renders the repository state.
    let report = knives::commands::preflight::gather(
        &knives::ids::RepoName::new("demo"),
        &entry,
        &mut store,
        &StateUnavailableForge,
    );
    let text = knives::commands::preflight::render(&report);

    // Then: the finding is visible and makes the command actionable to scripts.
    assert!(
        text.contains("!!") && text.contains("beyond the shared base"),
        "mixed base not rendered: {text}"
    );
    assert_eq!(
        knives::commands::preflight::exit_for(&report),
        knives::cli::Exit::Findings
    );
}

#[test]
fn a_cut_is_flat_and_carries_its_provenance() {
    // A release must be a flat merge of exactly the parents intended. The
    // failure this guards is silent: a cut that dropped a parent looks exactly
    // like one that did not, until work goes missing downstream.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha, "pull/10/head".to_owned()),
            (beta, "feat/beta".to_owned()),
        ],
    };
    let created =
        knives::commands::release::cut(&lab.work, &request, &ReleaseScheme::Dated).expect("cut");

    // Flat: exactly two parents, no nested integration node.
    let parents = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .parents_of(created.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2, "a release must be flat");

    // The dated name points at it, and the provenance rode along.
    let named = lab.revision(&lab.work, "release/2026-07-30", "commit_id");
    assert_eq!(named.trim(), created.as_str());
    let message = lab.revision(&lab.work, created.as_str(), "description");
    assert!(
        message.contains("from pull/10/head"),
        "provenance was lost: {message}"
    );
}

#[test]
fn a_cut_refuses_when_the_merge_did_not_get_the_parents_it_asked_for() {
    // The refusal was untested: the whole ensure! block could be deleted and the
    // flatness test still passed. jj dedupes duplicate parents, so asking for
    // the same commit twice produces a merge with fewer parents than requested,
    // which is exactly the "a branch's work was dropped" shape.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());

    let request = knives::commands::release::Cut {
        name: "release/2026-07-30".to_owned(),
        parents: vec![alpha.clone(), alpha.clone()],
        provenance: vec![(alpha, "feat/alpha".to_owned())],
    };
    let outcome = knives::commands::release::cut(&lab.work, &request, &ReleaseScheme::Dated);
    assert!(
        outcome.is_err(),
        "a parent-count mismatch must refuse, got {outcome:?}"
    );

    // And the dated name was not set on a bad cut. Asked of the bookmark list,
    // because resolving a bookmark that rightly does not exist is an error.
    let names = knives::jj::Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !names
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-07-30"),
        "a refused cut still claimed the name"
    );
}

#[test]
fn fixed_previous_position_keeps_the_published_remote_after_a_local_cut() {
    // Given: a fixed integration cut published to origin, then advanced only locally.
    let lab = lab::Lab::new();
    lab.branch("integration", "integration.txt", "published\n");
    lab.push_branch("integration");
    let published = Repo::open(&lab.work)
        .expect("open published repo")
        .resolve_commit("integration@origin")
        .expect("published integration tip");
    lab.jj_work(["new", "-r", "integration", "-m", "local integration cut"]);
    std::fs::write(lab.work.join("integration.txt"), "local cut\n").expect("write local cut");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: Some("integration".to_owned()),
        test_count_command: None,
        consumers: Vec::new(),
    };
    let repo = Repo::open(&lab.work).expect("open after local cut");
    let local = repo
        .resolve_commit("integration")
        .expect("local integration tip");

    // When: the previous fixed release position is read after the local cut.
    let previous = knives::commands::release::previous_position(&repo, &entry);

    // Then: it is the unchanged published remote, not the new local cut.
    assert_ne!(local, published);
    assert_eq!(previous, Some(("integration@origin".to_owned(), published)));
}

#[test]
fn a_fixed_release_branch_is_cut_in_place_and_its_previous_position_is_the_old_cut() {
    // Given: a fork with one feature branch and a fixed integration branch scheme.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration");
    let scheme = entry.release_scheme();

    // When: the first fixed cut is made and pushed.
    let opened = Repo::open(lab.work_path()).expect("open");
    let carried = knives::commands::release::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches");
    let trunk = opened
        .resolve_commit(&entry.upstream_trunk())
        .expect("upstream trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    let first = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("first cut");
    lab.push_branch("integration");
    lab.fetch_work();

    // MANDATORY reopen: Repo::open reads state at call time, and the first handle
    // predates the push/fetch that made integration@origin available locally.
    let opened = Repo::open(lab.work_path()).expect("reopen after fetch");
    let previous = knives::commands::release::previous_position(&opened, &entry)
        .expect("a pushed cut is a previous position");

    // Then: the remote-tracking ref is the old cut before any subsequent push.
    assert_eq!(
        previous,
        ("integration@origin".to_owned(), first.clone()),
        "the old cut is the previous release"
    );

    lab.branch("feat/beta", "beta.txt", "two\n");
    let opened = Repo::open(lab.work_path()).expect("reopen for second cut");
    let carried = knives::commands::release::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches for second cut");
    let trunk = opened
        .resolve_commit(&entry.upstream_trunk())
        .expect("upstream trunk for second cut");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    let second = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("second fixed cut may move integration sideways");
    let opened = Repo::open(lab.work_path()).expect("reopen after second cut");

    assert_eq!(
        opened
            .resolve_commit("integration")
            .expect("integration tip"),
        second,
        "the fixed bookmark advances to the fresh flat merge"
    );
    assert_eq!(
        knives::commands::release::previous_position(&opened, &entry),
        Some(("integration@origin".to_owned(), first)),
        "the still-unpushed second cut keeps the first published cut as previous"
    );
}

#[test]
fn a_dated_cut_refuses_a_sideways_bookmark_move() {
    // Given: two unrelated flat dated cuts.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    lab.branch("feat/beta", "beta.txt", "two\n");
    lab.octopus("release/2026-08-01", "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "three\n");
    lab.octopus("release/2026-08-02", "feat/alpha", "feat/gamma");
    let replacement = Repo::open(lab.work_path())
        .expect("open")
        .parents_of("release/2026-08-02")
        .expect("replacement dated cut parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect();

    // When: cut rebuilds the second merge under the first dated name.
    let moved = knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "release/2026-08-01".to_owned(),
            parents: replacement,
            provenance: vec![],
        },
        &ReleaseScheme::Dated,
    );

    // Then: Dated routing retains jj's sideways-move protection.
    assert!(moved.is_err(), "dated cuts must not move sideways");
}

#[test]
fn plan_for_a_fixed_release_ignores_a_non_publish_remote() {
    // Given: the same fixed release exists on both the publish remote and upstream.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "one\n");
    let entry = lab.repo_entry_with_release_branch("integration");
    let scheme = entry.release_scheme();
    let opened = Repo::open(lab.work_path()).expect("open");
    let carried = knives::commands::release::carried_branches(&opened, entry.trunk(), &scheme)
        .expect("carried branches");
    let trunk = opened
        .resolve_commit(&entry.upstream_trunk())
        .expect("upstream trunk");
    let mut parents = vec![trunk];
    parents.extend(carried.into_iter().map(|(_, commit)| commit));
    knives::commands::release::cut(
        lab.work_path(),
        &knives::commands::release::Cut {
            name: "integration".to_owned(),
            parents,
            provenance: vec![],
        },
        &scheme,
    )
    .expect("cut");
    lab.push_branch("integration");
    lab.jj_work(["bookmark", "track", "integration", "--remote", "upstream"]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "upstream",
        "--bookmark",
        "integration",
    ]);
    lab.fetch_work();
    let upstream = Repo::open(lab.work_path())
        .expect("reopen after fetch")
        .resolve_commit("integration@upstream");
    assert!(upstream.is_ok(), "upstream fixed release must be present");
    lab.jj_work(["bookmark", "delete", "integration"]);

    // When: planning selects the newest fixed release without a local bookmark.
    let plan = knives::commands::release::plan(&knives::ids::RepoName::new("a-repo"), &entry, &[])
        .expect("plan");

    // Then: upstream cannot be mistaken for the publish remote's release.
    assert_eq!(plan.release.as_deref(), Some("integration@origin"));
}

#[test]
fn status_with_the_landed_probe_reports_a_merged_branch_and_leaves_no_trace() {
    // The probe path through `knives status` end to end. It is exercised here and
    // deliberately never against a live shared repository, because it mutates.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);

    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    let before = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: true,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");
    let after = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");

    // The merged branch reads as already in the trunk. Stated on the branch itself
    // rather than as a finding: it is a fact about the branch, not something wrong.
    let verdicts: Vec<_> = report
        .branches
        .iter()
        .filter_map(|row| row.landed)
        .collect();
    assert!(
        verdicts.contains(&knives::detect::landed::LandedVerdict::InTrunk),
        "expected the squash-merged branch to read as in-trunk: {verdicts:?}"
    );

    // And the probe left the repository as it found it.
    assert_eq!(before, after, "the probe left commits behind");
}

#[test]
fn status_reports_branch_overlap_after_upstream_advances_without_landed_probe() {
    // Given: two maintained branches from a trunk revision that upstream has since advanced past
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.advance_upstream("upstream advanced past the branches\n");
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let store_path = lab
        .work
        .parent()
        .expect("lab work directory has a parent")
        .join("state.json");
    let store = knives::store::Store::open(store_path).expect("store");
    let name = knives::ids::RepoName::new("a-repo");

    // When: status deliberately skips only the landed replay
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");

    // Then: the independent path comparison still reports the shared file
    let overlap = report
        .findings
        .iter()
        .find(|finding| {
            finding.kind == knives::detect::FindingKind::BranchOverlap
                && finding.subject == knives::detect::Subject::File("shared.txt".to_owned())
        })
        .expect("the shared file is reported even without the landed probe");
    assert!(overlap.detail.contains("feat/alpha"), "was: {overlap:?}");
    assert!(overlap.detail.contains("feat/beta"), "was: {overlap:?}");
}

#[test]
fn status_reports_a_branch_carried_elsewhere() {
    // Given: an open branch whose tip is an ancestor of another local bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status gathers the branch report
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");

    // Then: the branch fact names the reference that reaches its tip
    assert!(report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
            && finding.detail.contains("theirs/rework")
    }));
}

#[test]
fn status_reports_a_carrier_for_a_closed_pull_request() {
    // Given: a closed pull request whose branch is reachable from another bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");
    let forge = knives::forge::FakeForge {
        pull_requests: std::iter::once((
            BranchName::new("feat/alpha"),
            knives::forge::PullRequest {
                number: 7,
                state: "CLOSED".to_owned(),
                review_decision: String::new(),
                head_ref_name: "feat/alpha".to_owned(),
                head_ref_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                updated_at: "2026-01-01T00:00:00Z".to_owned(),
                is_draft: false,
                url: String::new(),
                head_repository_owner: None,
                mergeable: String::new(),
                merge_state_status: String::new(),
                base_ref_name: "main".to_owned(),
                merge_commit: None,
            },
        ))
        .collect(),
        ..knives::forge::FakeForge::default()
    };

    // When: status gathers the branch report with the closed pull request
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            registry: None,
        },
    )
    .expect("gather");

    // Then: forge state does not suppress the local ancestry fact
    assert!(report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
            && finding.detail.contains("theirs/rework")
    }));
}

#[test]
fn status_does_not_report_trunk_as_a_carrier_without_landed_probe() {
    // Given: a branch whose tip is reachable from the local trunk bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "feat/alpha",
        "-m",
        "trunk carries feature",
    ]);
    lab.jj_work(["bookmark", "set", "main", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status skips the landed probe
    let report = knives::commands::status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
        },
    )
    .expect("gather");

    // Then: trunk is never a carrier finding, even without an InTrunk verdict
    assert!(!report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subject == knives::detect::Subject::Branch(BranchName::new("feat/alpha"))
    }));
}

#[test]
fn a_fresh_cut_carries_every_branch_and_nothing_else() {
    // What a dated release is: a flat merge of the current tip of everything we
    // carry. Not the trunk, not other releases.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-07-29", "feat/alpha", "feat/beta");

    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let carried = knives::commands::release::carried_branches(&repo, "main", &ReleaseScheme::Dated)
        .expect("carried");
    let names: Vec<&str> = carried.iter().map(|(branch, _)| branch.as_str()).collect();

    assert!(names.contains(&"feat/alpha"));
    assert!(names.contains(&"feat/beta"));
    assert!(
        !names.iter().any(|n| n.starts_with("release/")),
        "a release is not a branch we carry"
    );
    assert!(
        !names.contains(&"main"),
        "the trunk is not a branch we carry"
    );
}

#[test]
fn cutting_a_release_reaps_the_superseded_one() {
    // Given: an existing cut and a consumer following it by branch.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: a newer release is cut through the binary.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the superseded cut is reaped while the newer one remains.
    assert!(
        output.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    let tips = Repo::open(&lab.work)
        .expect("reopen release repository")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04")
    );
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );
}

#[test]
fn a_named_cut_with_an_inconclusive_content_audit_returns_findings() {
    // Given: two members entangled in one file, so the cut is conflicted from birth
    // and every member's replay onto it answers nothing either way.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);

    // When: the cut is named despite its deliberately non-fatal audit result.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is retained, but automation receives the unresolved finding.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("content check inconclusive"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Repo::open(&lab.work)
            .expect("open named cut")
            .resolve_commit("release/2026-08-05")
            .is_ok(),
        "the inconclusive cut was not named"
    );
}

#[test]
fn a_named_cut_that_drops_the_test_count_returns_findings() {
    // Given: one member reports ten tests while another makes the merged tree
    // report five. The check compares the cut against a single contributing
    // branch — the first parent — so the low-count member must not be first.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "branch-count", "5\n");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\ntest_count_command = \"if test -f branch-count; then cat branch-count; else printf 10; fi\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("configure test counter");

    // When: the real cut command observes the lower count in its new tree.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut remains named but reports the dropped-suite finding to automation.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("dropped that branch's tests"), "{stdout}");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_rebase_refuses_when_every_pin_is_frozen() {
    // Given: a dated release whose only consumer pins it by revision. Moving the
    // bookmark in place would reach nobody, so this requires a new dated cut.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write registry");
    lab.advance_upstream("upstream advance\n");
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("resolve release before refusal");

    // When: the real binary is asked to rebase the release onto upstream.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it directs the caller to a dated cut, exits incomplete, and does not move it.
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "frozen-pin guidance missing: {stdout}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release after refusal");
    assert_eq!(before, after, "a frozen release was moved in place");
}

#[test]
fn release_rebase_refusal_for_fixed_release_explains_that_revision_pins_cannot_follow_it() {
    // Given: a fixed release branch whose only consumer pin is a frozen revision.
    let lab = Lab::new();
    let release = "integration";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "# checkout pin\nwork = { git = \"https://forge.invalid/acme/work.git\", rev = \"integration\" }\n",
        "# origin pin\nwork = { git = \"https://forge.invalid/acme/work.git\", rev = \"integration\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write fixed-release registry");
    lab.advance_upstream("upstream advance\n");

    // When: the fixed release is asked to move in place.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it is incomplete and names the only viable remediation.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("update the frozen consumer pins")
            && stdout.contains("fixed branches cannot reach revision pins"),
        "fixed-scheme guidance missing: {stdout}"
    );
}

#[test]
fn release_rebase_repairs_a_followed_dated_release_with_a_sideways_merge() {
    // Given: an existing dated release, a consumer that follows it, and a new upstream commit.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let previous_parents = Repo::open(&lab.work)
        .expect("open release repository")
        .parents_of(release)
        .expect("read existing release parents");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let upstream = Repo::open(&lab.work)
        .expect("reopen release repository")
        .resolve_commit("main@upstream")
        .expect("resolve advanced upstream");
    // When: the repair command moves the existing release onto a new flat merge.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: the command succeeds; the legacy trunk parent is shed — the base is
    // never a parent — and the members were rebased onto the new upstream,
    // their bookmarks following.
    assert!(
        output.status.success(),
        "release rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let repo = Repo::open(&lab.work).expect("reopen repaired release repository");
    let parents = release_parent_commits(&lab, release);
    let old_base = &previous_parents[0]; // lab.octopus puts main@origin first
    assert!(
        !parents.contains(&old_base.commit),
        "superseded base still a parent: {parents:?}"
    );
    assert!(
        !parents.contains(&upstream),
        "the new upstream must not become a parent either: {parents:?}"
    );
    assert_eq!(
        parents,
        vec![commit_at(&lab, "feat/alpha"), commit_at(&lab, "feat/beta")],
        "the rewritten members are the whole parent set"
    );
    assert!(
        repo.is_ancestor(&upstream, &commit_at(&lab, release))
            .expect("ancestry"),
        "the release must contain the upstream through its members"
    );
}

#[test]
fn a_second_rebase_does_not_grow_the_release() {
    // Given: a release rebased once already, and upstream advancing again.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is rebased after each upstream advance.
    for advance in ["first advance\n", "second advance\n"] {
        lab.advance_upstream(advance);
        let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);
        assert!(
            output.status.success(),
            "rebase failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Then: two parents — the members — whatever the rebase count. The first
    // rebase sheds the legacy trunk parent and no rebase adds one back.
    let parents = Repo::open(&lab.work)
        .expect("open")
        .parents_of(release)
        .expect("parents");
    assert_eq!(parents.len(), 2, "parents accumulated: {parents:?}");
}

#[test]
fn a_rebase_refuses_a_stale_parent_it_cannot_map() {
    // Given: a standard release whose captured alpha tip is no longer held by alpha.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let stale_parent = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve captured alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["new", "-r", "feat/alpha"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release before refusal");

    // When: the real binary attempts to replace the release base.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it refuses rather than carrying the stale parent or moving the release.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains(&stale_parent.as_str()[..12]),
        "refusal must name the stale parent: {text}"
    );
    assert!(
        text.contains("feat/alpha (now "),
        "refusal must say where alpha moved: {text}"
    );
    let after = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit(release)
        .expect("resolve release after refusal");
    assert_eq!(before, after, "refusal moved the release");
}

#[test]
fn a_rebase_keeps_a_landed_bookmark_held_member() {
    // Given: alpha is both held by its bookmark and reachable from the explicit replacement.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let alpha = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work([
        "new",
        "-r",
        "main@upstream",
        "-r",
        "feat/alpha",
        "-m",
        "upstream merge carrying alpha",
    ]);
    let replacement = lab.revision(&lab.work, "@", "commit_id");
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: rebase uses that merge as its explicit replacement base.
    let output = knives_release(&lab, &home, &["rebase", replacement.trim()]);

    // Then: alpha remains a direct member parent despite being reachable from replacement.
    assert!(
        output.status.success(),
        "rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = Repo::open(&lab.work)
        .expect("reopen")
        .parents_of(release)
        .expect("read repaired parents");
    assert_eq!(parents.len(), 2, "parents: {parents:?}");
    assert!(
        parents.iter().any(|parent| parent.commit == alpha),
        "landed alpha was dropped: {parents:?}"
    );
}

#[test]
fn a_rebase_moves_the_whole_composition_onto_the_target() {
    // Given: two members forked from the old trunk, their merge conflict
    // resolved on the release, and an upstream that has advanced. `rebase` is
    // `jj rebase -b <release> -d <target>`: the members move onto the target
    // and the release moves with them — the trunk never becomes a parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first cut")
            .resolve_commit("release/2026-08-04")
            .is_ok(),
        "first cut was not named: {first:?}"
    );
    lab.jj_work(["new", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "resolved\n").expect("resolve conflict");
    lab.jj_work(["squash"]);
    let old_alpha = commit_at(&lab, "feat/alpha");
    lab.advance_upstream("advance\n");

    // When: the composition is rebased onto the advanced trunk.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);
    assert!(
        output.status.success(),
        "rebase failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the members were rewritten onto the new trunk and their bookmarks
    // followed; the release's parents are exactly the moved members.
    let repo = Repo::open(&lab.work).expect("reopen after rebase");
    let trunk = commit_at(&lab, "main@upstream");
    let new_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(new_alpha, old_alpha, "alpha was not rewritten");
    assert!(
        repo.is_ancestor(&trunk, &new_alpha).expect("ancestry"),
        "alpha does not sit on the new trunk"
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(parents.contains(&new_alpha), "{parents:?}");
    assert!(
        !parents.contains(&trunk),
        "the trunk must not become a parent: {parents:?}"
    );
    assert_eq!(parents.len(), 2, "{parents:?}");
    assert!(
        repo.is_ancestor(&trunk, &commit_at(&lab, "release/2026-08-04"))
            .expect("ancestry"),
        "the release does not contain the new trunk through its members"
    );
    // And: the recorded resolution replayed through the rebase.
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "shared.txt"),
        "resolved\n"
    );
}

/// Run the knives binary's release command with a fake forge CLI on the PATH:
/// `pr list` answers from `pulls`, and any other forge call fails the test loudly.
/// The bare rebase default is the only release path that consults the forge.
fn knives_release_with_forge(
    lab: &Lab,
    home: &tempfile::TempDir,
    pulls: &str,
    args: &[&str],
) -> std::process::Output {
    let shim = tempfile::tempdir().expect("create forge shim directory");
    let payload = shim.path().join("pulls.json");
    std::fs::write(&payload, pulls).expect("write pull request payload");
    let gh = shim.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *\" pr list \"*) cat \"{}\" ;;\n  *) echo \"unexpected gh invocation: $*\" >&2; exit 1 ;;\nesac\n",
            payload.display()
        ),
    )
    .expect("write gh shim");
    let mut permissions = std::fs::metadata(&gh)
        .expect("read gh shim permissions")
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&gh, permissions).expect("make gh shim executable");
    let path = std::env::join_paths(std::iter::once(shim.path().to_owned()).chain(
        std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set")),
    ))
    .expect("construct shim PATH");
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "release", "--repo", "demo"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("PATH", path)
        .output()
        .expect("run knives release with a forge shim")
}

/// One forge pull request record with the fields the binary requires.
fn pull_record(number: u64, state: &str, branch: &str, merge_oid: Option<&str>) -> String {
    let merge = merge_oid.map_or_else(String::new, |oid| {
        format!(",\"mergeCommit\":{{\"oid\":\"{oid}\"}}")
    });
    format!(
        "{{\"number\":{number},\"state\":\"{state}\",\"headRefName\":\"{branch}\",\
         \"headRefOid\":\"0123456789abcdef0123456789abcdef01234567\",\
         \"updatedAt\":\"2026-08-07T00:00:00Z\",\"baseRefName\":\"main\"{merge}}}"
    )
}

#[test]
fn a_bare_rebase_with_no_merged_pull_request_requires_a_commit() {
    // Given: a release in hand, and a forge whose only pull request is still open.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let before = commit_at(&lab, release);
    let pulls = format!("[{}]", pull_record(7, "OPEN", "feat/alpha", None));

    // When: the bare rebase asks the forge for a default target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: with nothing merged there is no default, and the release stays put.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: no pull request has merged, so there is no default target; \
             provide a commit to rebase onto"
        ),
        "missing refusal guidance: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_targets_the_trunk_commit_that_holds_every_merged_pull_request() {
    // Given: alpha merged upstream by a merge commit, beta still open, and the
    // trunk advanced past that merge afterwards.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond the merge\n");
    let tip = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str())),
        pull_record(8, "OPEN", "feat/beta", None)
    );

    // When: the bare rebase asks the forge instead of taking a target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the release lands on the merge commit, not the later tip.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: every merged pull request (#7) is in main@upstream by {}; rebasing onto it",
            &merged_at.as_str()[..12]
        )),
        "missing target explanation: {stdout}"
    );
    let repo = Repo::open(&lab.work).expect("reopen after the default rebase");
    let at_release = commit_at(&lab, release);
    assert!(
        repo.is_ancestor(&merged_at, &at_release).expect("ancestry"),
        "the release does not contain the merge commit"
    );
    assert!(
        !repo.is_ancestor(&tip, &at_release).expect("ancestry"),
        "the release overshot the merged point onto the trunk tip"
    );
}

#[test]
fn a_bare_rebase_covers_the_latest_of_several_merged_pull_requests() {
    // Given: alpha and gamma merged upstream in that order, so gamma's merge
    // commit is the first trunk commit containing both.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/gamma", 8);
    lab.merge_pull_with_merge_commit(8);
    let second_merge = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond both merges\n");
    let tip = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/gamma", Some(second_merge.as_str()))
    );

    // When: the bare rebase chooses among several merged pull requests.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the later merge commit wins because it contains the earlier one.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: every merged pull request (#7, #8) is in main@upstream by {}; \
             rebasing onto it",
            &second_merge.as_str()[..12]
        )),
        "missing target explanation: {stdout}"
    );
    let repo = Repo::open(&lab.work).expect("reopen after the default rebase");
    let at_release = commit_at(&lab, release);
    assert!(
        repo.is_ancestor(&second_merge, &at_release)
            .expect("ancestry"),
        "the release does not contain the covering merge commit"
    );
    assert!(
        !repo.is_ancestor(&tip, &at_release).expect("ancestry"),
        "the release overshot the merged point onto the trunk tip"
    );
}

#[test]
fn a_bare_rebase_refuses_merged_work_missing_from_the_local_trunk() {
    // Given: the forge says a pull request merged, but its merge commit is not
    // in the local repository at all.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let before = commit_at(&lab, release);
    let pulls = format!(
        "[{}]",
        pull_record(
            7,
            "MERGED",
            "feat/alpha",
            Some("feedfacefeedfacefeedfacefeedfacefeedface")
        )
    );

    // When: the bare rebase tries to place that merge on the local trunk.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: it refuses with fetch guidance rather than guessing, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "demo: the merge commit(s) of #7 are not in the local main@upstream; \
             run knives sync, or provide a commit to rebase onto"
        ),
        "missing fetch guidance: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_bare_rebase_drops_a_member_whose_pull_request_landed() {
    // Given: alpha merged upstream by a merge commit and beta still open.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    lab.advance_upstream("beyond the merge\n");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str())),
        pull_record(8, "OPEN", "feat/beta", None)
    );

    // When: the bare rebase lands on the merge commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha's parent is dropped, its bookmark untouched, and the drop is
    // recorded on the release itself.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dropped feat/alpha: landed upstream as #7"),
        "missing drop report: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release),
        vec![commit_at(&lab, "feat/beta")],
        "the landed member must be the only parent removed"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen")
            .resolve_commit("feat/alpha")
            .is_ok(),
        "dropping the parent must not touch the branch"
    );
    assert!(
        lab.revision(&lab.work, release, "description")
            .contains("dropped feat/alpha: landed upstream as #7"),
        "the release description must record the drop"
    );
}

#[test]
fn no_drop_keeps_a_landed_member_as_a_parent() {
    // Given: the same landed alpha, and the caller asking to keep it.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let merged_at = commit_at(&lab, "main@upstream");
    let alpha = commit_at(&lab, "feat/alpha");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase runs with --no-drop.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase", "--no-drop"]);

    // Then: the landed member stays a parent and nothing reports a drop.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!stdout.contains("dropped feat/alpha"), "{stdout}");
    assert!(
        release_parent_commits(&lab, release).contains(&alpha),
        "--no-drop must keep the landed member"
    );
}

#[test]
fn a_bare_rebase_drops_a_member_landed_by_squash() {
    // Given: alpha squash-merged, so its commits replay empty onto the target
    // without the tip ever entering the trunk's history.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase lands on the squash commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha is dropped and beta's rewritten tip is the only parent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dropped feat/alpha: landed upstream as #7"),
        "missing drop report: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release),
        vec![commit_at(&lab, "feat/beta")],
        "the squash-landed member must be dropped"
    );
}

#[test]
fn a_bare_rebase_keeps_a_landed_branch_that_carries_work_past_its_pull() {
    // Given: alpha's pull request holds only its first commit, the branch has a
    // second commit past it, and the release carries the extended tip. The pull
    // then squash-merges, so the branch still holds work the trunk lacks.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.publish_pull("feat/alpha", 7);
    extend_branch(&lab, "feat/alpha", "alpha-more.txt", "work past the pull\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.squash_merge_pull(7, None);
    let merged_at = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(merged_at.as_str()))
    );

    // When: the bare rebase lands on the squash commit.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: alpha is kept, with the reason stated, and both members remain.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "rebase failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("kept feat/alpha: it carries work past #7"),
        "missing keep report: {stdout}"
    );
    assert!(!stdout.contains("dropped feat/alpha"), "{stdout}");
    assert_eq!(
        release_parent_commits(&lab, release).len(),
        2,
        "both members must remain parents"
    );
}

#[test]
fn a_rebase_refuses_a_composition_whose_every_member_landed() {
    // Given: a single-member release whose only branch merged upstream. jj
    // would re-parent the release straight onto the destination, leaving the
    // trunk as its only parent — and the base is never a parent.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", release]);
    assert!(cut.status.success(), "{cut:?}");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    let before = commit_at(&lab, release);

    // When: the rebase is pointed past the landing, explicitly.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: it refuses rather than making the trunk a parent, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(
            "every member of release/2026-08-04 has landed in main@upstream; rebasing would \
             make the trunk the only parent, so nothing moved \u{2014} reap the release or \
             include new work"
        ),
        "missing fully-landed refusal: {stdout}"
    );
    assert_eq!(before, commit_at(&lab, release), "the release moved anyway");
}

#[test]
fn a_release_already_at_its_target_still_drops_landed_members() {
    // Given: both members squash-merged and the release already rebased onto
    // their covering commit with --no-drop, so it carries two landed members
    // as empty replayed chains.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);
    let first_merge = commit_at(&lab, "main@upstream");
    lab.publish_pull("feat/beta", 8);
    lab.squash_merge_pull(8, None);
    let second_merge = commit_at(&lab, "main@upstream");
    let pulls = format!(
        "[{},{}]",
        pull_record(7, "MERGED", "feat/alpha", Some(first_merge.as_str())),
        pull_record(8, "MERGED", "feat/beta", Some(second_merge.as_str()))
    );
    let kept = knives_release_with_forge(&lab, &home, &pulls, &["rebase", "--no-drop"]);
    assert!(kept.status.success(), "{kept:?}");

    // When: the bare rebase finds the release already contains the target.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: nothing moves, and dropping every landed member is refused because
    // it would leave the release without a parent.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("already contains"),
        "missing already-contains skip: {stdout}"
    );
    assert!(
        stdout.contains(
            "every member of release/2026-08-04 landed; dropping them all would leave it \
             without a parent, so nothing was dropped"
        ),
        "missing parentless-drop refusal: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release).len(),
        2,
        "a refused drop must leave the parents alone"
    );
}

#[test]
fn a_release_already_containing_the_reference_by_ancestry_is_left_alone() {
    // Given: a release whose alpha parent merged the upstream advance, so the
    // seed trunk is reachable through alpha rather than a direct release parent.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let seed = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("main@upstream")
        .expect("seed tip");
    lab.advance_upstream("advance\n");
    lab.jj_work([
        "new",
        "feat/alpha",
        "main@upstream",
        "-m",
        "merge upstream into alpha",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.jj_work(["new", "-r", "feat/alpha", "-r", "feat/beta", "-m", release]);
    lab.jj_work(["bookmark", "create", release, "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let before = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release");

    // When: asked to include the seed, an ancestor of alpha's merged base.
    let output = knives_release(&lab, &home, &["rebase", seed.as_str()]);

    // Then: containment is recognized and the release does not move.
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("already contains"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let after = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit(release)
        .expect("release");
    assert_eq!(
        before, after,
        "release moved for an already-contained commit"
    );
}

#[test]
fn a_release_contains_the_trunk_through_a_parents_history_not_as_a_direct_parent() {
    // Given: a member branch that merged the advanced upstream, and a release
    // whose direct parents are the seed trunk, that branch, and another branch.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.advance_upstream("advance\n");
    lab.jj_work([
        "new",
        "feat/alpha",
        "main@upstream",
        "-m",
        "merge upstream into alpha",
    ]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.octopus(release, "feat/alpha", "feat/beta");

    // When/Then: the probe sees containment through ancestry.
    let repo = Repo::open(&lab.work).expect("open");
    assert_eq!(
        knives::commands::release::trunk_lag(&repo, Some(release), "main@upstream"),
        None,
        "trunk is contained through feat/alpha's merge; the probe must not report lag"
    );
}

#[test]
fn preflight_reports_main_when_a_repo_configures_dev_as_its_trunk() {
    // Given: an upstream whose trunk is dev while main is a local work branch.
    let lab = lab::Lab::new();
    lab.jj_work(["bookmark", "set", "dev", "-r", "main"]);
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: Some("dev".to_owned()),
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };

    // When: preflight collects locally maintained branches.
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");

    // Then: dev is the only excluded trunk; main remains work to report.
    assert!(
        states.iter().any(|state| state.branch == "main"),
        "a non-trunk main branch must be reported, got {states:#?}"
    );
    assert!(
        !states.iter().any(|state| state.branch == "dev"),
        "the configured trunk is not a branch we maintain, got {states:#?}"
    );
}

#[test]
fn preflight_treats_a_fixed_release_branch_as_a_release_not_a_branch() {
    // Given: a fixed release bookmark and ordinary feature work.
    let lab = lab::Lab::new();
    lab.jj_work(["bookmark", "set", "integration", "-r", "main"]);
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: Some("integration".to_owned()),
        test_count_command: None,
        consumers: Vec::new(),
    };

    // When: preflight collects locally maintained branches.
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");

    // Then: the fixed cut is excluded while feature work remains visible.
    assert!(
        !states.iter().any(|state| state.branch == "integration"),
        "a fixed release is not a branch to preflight, got {states:#?}"
    );
    assert!(
        states.iter().any(|state| state.branch == "feat/alpha"),
        "feature work must still be preflighted, got {states:#?}"
    );
}

#[test]
fn preflight_hides_a_divergent_configured_trunk_bookmark() {
    // Given: the configured trunk has independently rewritten local and origin tips.
    let lab = lab::Lab::new();
    lab.branch("dev", "dev.txt", "dev\n");
    lab.rewrite_in_both_clones("dev");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: Some("dev".to_owned()),
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };

    // When: preflight reads divergent bookmarks before regular branch tips.
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");

    // Then: the trunk is excluded even when it is divergent.
    assert!(
        !states
            .iter()
            .any(|state| state.branch == "dev" && state.divergent),
        "the trunk must not appear as divergent work, got {states:#?}"
    );
}

#[test]
fn preflight_flags_a_branch_whose_tip_is_divergent() {
    // Pins a bug that shipped silently: divergence findings carry a CHANGE id,
    // and comparing them against a branch tip's COMMIT id never matches, so
    // nothing was ever reported as divergent.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.rewrite_in_both_clones("feat/alpha");

    let entry = knives::config::RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");
    assert!(
        states.iter().any(|state| state.divergent),
        "a branch whose tip is divergent must be reported as divergent, got {states:#?}"
    );
}

#[test]
fn the_probe_never_abandons_a_commit_it_did_not_create() {
    // Reproduction of a data-loss defect found in review. The cleanup used to
    // identify its own commits by set difference over children(onto). A dirty
    // `@` that is a child of `onto` has its commit id rewritten by any
    // snapshotting command, so it appeared in that difference and was abandoned.
    // Three commits and two bookmarks of another agent's work were destroyed by
    // a single `knives status`. Cleanup now abandons only ids jj reported creating.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");

    // Another agent's work, sitting as a child of the trunk with a dirty tree.
    lab.jj_work(["new", "main", "-m", "SOMEONE ELSE MID-TASK"]);
    std::fs::write(lab.work.join("their-wip.txt"), "precious\n").expect("write");
    lab.jj_work(["bookmark", "create", "their-work", "-r", "@"]);
    let theirs = lab.revision(&lab.work, "their-work", "commit_id");

    let _ = knives::jj::probe_landed(
        &lab.work,
        &knives::ids::BranchName::new("feat/alpha"),
        "main",
    );

    // Their commit, their bookmark and their file all survive.
    let still_there = lab.revision(&lab.work, "their-work", "commit_id");
    assert_eq!(
        still_there.trim(),
        theirs.trim(),
        "the probe abandoned another agent's commit"
    );
    assert!(
        lab.work.join("their-wip.txt").exists(),
        "the probe destroyed another agent's uncommitted file"
    );
}

#[test]
fn cutting_a_release_does_not_move_another_agents_working_copy() {
    // Reproduction of a defect found in review. `create_merge` used `jj new`,
    // which moves `@`, so cutting a release parked whoever was working in the
    // repo's default workspace on top of the release octopus with their
    // uncommitted edits pending against it. That is verbatim the accident
    // `knives start` exists to prevent, caused by `knives release`.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");

    lab.jj_work(["new", "main", "-m", "SOMEONE ELSE MID-TASK"]);
    std::fs::write(lab.work.join("their-wip.txt"), "in progress\n").expect("write");
    let before = lab.revision(&lab.work, "@", "change_id");

    let alpha =
        knives::ids::CommitId::new(lab.revision(&lab.work, "feat/alpha", "commit_id").trim());
    let beta = knives::ids::CommitId::new(lab.revision(&lab.work, "feat/beta", "commit_id").trim());
    let _ = knives::jj::create_merge(&lab.work, &[alpha, beta], "release: test").expect("merge");

    let after = lab.revision(&lab.work, "@", "change_id");
    assert_eq!(
        before, after,
        "cutting a release moved someone else's working copy"
    );
    assert!(
        lab.work.join("their-wip.txt").exists(),
        "their uncommitted file vanished"
    );
}

#[test]
fn a_stranded_release_parent_reports_where_the_branch_went() {
    // The payload the design asks for. `parents_of` only ever reports bookmarks
    // pointing AT a parent, so the pure detector can only ever say "carries no
    // bookmark". Naming the branch and its new tip is the actionable half.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let stranded = lab.revision(&lab.work, "feat/alpha", "commit_id");

    // The branch moves on, leaving the old commit with nothing pointing at it.
    lab.jj_work(["new", "feat/alpha", "-m", "more work"]);
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    let moved_to = lab.revision(&lab.work, "feat/alpha", "commit_id");
    assert_ne!(stranded.trim(), moved_to.trim());

    let past = knives::jj::branches_past(&lab.work, &knives::ids::CommitId::new(stranded.trim()))
        .expect("branches past");
    assert!(
        past.iter().any(|(branch, tip)| branch.as_str() == "feat/alpha" && tip.as_str() == moved_to.trim()),
        "expected feat/alpha reported at its new tip, got {past:?}"
    );
}

#[test]
fn a_foreign_pull_request_can_be_fetched_and_carried_as_a_release_parent() {
    // The design allows a release parent to be any upstream pull request, not
    // only our own branches. Without the objects locally that commit cannot be a
    // merge parent at all, and none of the obvious fetch routes work: jj brings
    // branches only, and importing a raw pull ref leaves the commit invisible.
    let lab = lab::Lab::new();
    lab.branch("feat/theirs", "theirs.txt", "someone else's work\n");
    lab.push_branch("feat/theirs");
    lab.publish_pull("feat/theirs", 42);
    let sha = lab.revision(&lab.work, "feat/theirs", "commit_id");

    // A clone that has never seen the branch, only the pull ref.
    let fetched = knives::jj::fetch_pull_ref(&lab.second, &lab.upstream.display().to_string(), 42)
        .expect("fetch pull ref");
    assert_eq!(fetched.as_str(), sha.trim(), "fetched the wrong commit");

    // And it is usable as a parent, which is the whole point.
    let trunk = knives::ids::CommitId::new(lab.revision(&lab.second, "main", "commit_id").trim());
    let merge =
        knives::jj::create_merge(&lab.second, &[trunk, fetched], "release: with a foreign PR")
            .expect("merge");
    let parents = knives::jj::Repo::open(&lab.second)
        .expect("open")
        .parents_of(merge.as_str())
        .expect("parents");
    assert_eq!(parents.len(), 2);
}

#[test]
fn a_cut_refuses_when_release_like_described_work_lives_only_in_the_release_lineage() {
    // Given: a real hotfix uses a release-like description while stacked on the
    // old release. The next flat cut would not include it, and no keeper reaches it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "new",
        "release/2026-08-04",
        "-m",
        "chore(release): restore missing file",
    ]);
    std::fs::write(lab.work_path().join("hotfix.txt"), "fix\n").expect("write hotfix");
    lab.jj_work(["new"]); // park @ off the hotfix so it snapshots as its own commit
    let stacked = lab.revision(
        lab.work_path(),
        "description(glob:\"hotfix*\")",
        "commit_id",
    );
    let (home, _consumer) = release_test_home(&lab);

    // When: a newer cut is attempted without acknowledgement.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: refused, naming the exact commit, and no new bookmark exists.
    assert!(!output.status.success(), "cut should have refused");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains(&stacked.chars().take(12).collect::<String>()),
        "refusal must name the commit: {text}"
    );
    assert!(
        text.contains("--allow-drop"),
        "refusal must name the override: {text}"
    );
    let tips = Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05")
    );

    // And when: the operator states that dropping the hotfix is intended.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05", "--allow-drop"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen after overridden cut")
        .bookmark_tips()
        .expect("read bookmark tips after overridden cut");
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-05"),
        "the acknowledged cut must create the requested release"
    );
}

#[test]
fn a_dropped_branch_does_not_trip_the_orphan_gate() {
    // Given: a member dropped from the release. Its bookmark still holds its
    // content, so nothing is lost and the gate must stay quiet.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "not this time"],
    );
    assert!(dropped.status.success(), "{dropped:?}");

    // When: the next cut is made without --allow-drop.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: it cuts, because feat/beta's bookmark still reaches its commits.
    assert!(
        output.status.success(),
        "gate tripped on a dropped branch: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // And: the drop it was given really removed something. A gate that stays
    // quiet over an unchanged release proves nothing about dropped content.
    let beta = commit_at(&lab, "feat/beta");
    assert!(
        !release_parent_commits(&lab, "release/2026-08-05").contains(&beta),
        "the dropped branch is still a parent of the successor cut"
    );
}

#[test]
fn include_adds_one_parent_and_changes_nothing_else() {
    // Given: a cut made before feat/gamma existed. Including gamma is one new
    // parent; every other parent stays at the commit the release already has.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    // When: the branch is included.
    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(
        output.status.success(),
        "include failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: exactly one parent was added and none moved.
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(after.len(), before.len() + 1, "{before:?} -> {after:?}");
    for parent in &before {
        assert!(
            after.contains(parent),
            "an existing parent moved: {before:?} -> {after:?}"
        );
    }
    let gamma = commit_at(&lab, "feat/gamma");
    assert!(after.contains(&gamma), "gamma tip missing: {after:?}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "gamma.txt"),
        "gamma\n"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "now has {} parent(s): included feat/gamma",
            after.len()
        )),
        "the reported parent count and delta must match the release: {stdout}"
    );
}

#[test]
fn including_a_carried_branch_is_a_reported_noop() {
    // Including a parent the release already has changes nothing at all — not
    // the parent set, and not the release commit either. A no-op that still
    // duplicated the release would churn its identity under every consumer.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let before = release_parent_commits(&lab, "release/2026-08-04");
    let before_commit = commit_at(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("already carries feat/alpha"), "{stdout}");
    assert_eq!(release_parent_commits(&lab, "release/2026-08-04"), before);
    assert_eq!(
        commit_at(&lab, "release/2026-08-04"),
        before_commit,
        "a reported no-op rewrote the release"
    );
}

#[test]
fn include_refuses_to_advance_an_advanced_branch() {
    // Given: a released branch that has advanced. Moving a member to its tip is
    // a content change beyond "include this", so it only happens when asked
    // for by name: `advance`.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("the branch has advanced")
            && stdout.contains("knives release advance feat/alpha"),
        "the refusal must name the verb that does move a member: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "include must not move a member"
    );
}

#[test]
fn drop_removes_one_parent_and_records_why() {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let beta = repo.resolve_commit("feat/beta").expect("beta tip");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/beta", "--why", "beta is not ready"],
    );
    assert!(
        output.status.success(),
        "drop failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dropped feat/beta: beta is not ready"),
        "the reported delta must carry the stated reason: {stdout}"
    );

    // Then: only beta's parent left; the reason is on the release itself; the
    // branch bookmark still holds the dropped work.
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(after.len(), before.len() - 1, "{before:?} -> {after:?}");
    assert!(!after.contains(&beta), "beta parent survived: {after:?}");
    assert!(after.contains(&alpha), "alpha parent vanished: {after:?}");
    let description = Repo::open(&lab.work)
        .expect("reopen")
        .description_of("release/2026-08-04")
        .expect("release description");
    assert!(description.contains("beta is not ready"), "{description}");
    let tips = Repo::open(&lab.work)
        .expect("reopen for tips")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys()
            .any(|reference| reference.branch().as_str() == "feat/beta"),
        "dropping a member must not touch its bookmark"
    );
}

#[test]
fn drop_resolves_an_advanced_branchs_parent_by_ancestry() {
    // Given: the release carries an old tip of feat/beta and the bookmark has
    // moved on. The name still resolves: the member parent is the one the
    // branch tip descends from.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released_beta = commit_at(&lab, "feat/beta");
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let repo = Repo::open(&lab.work).expect("reopen after beta advanced");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["drop", "feat/beta", "--why", "superseded"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        !after.contains(&released_beta),
        "the ancestor parent survived: {after:?}"
    );
    assert_eq!(after.len(), before.len() - 1, "{before:?} -> {after:?}");
    assert!(
        after.contains(&alpha),
        "the drop removed more than the named member: {after:?}"
    );
}

#[test]
fn advance_moves_only_the_named_parents_then_all() {
    // Given: both members have advanced. Advancing is a content change beyond
    // any single include, so it moves exactly what was named, and everything
    // only when nothing is.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let repo = Repo::open(&lab.work).expect("open");
    let old_alpha = repo.resolve_commit("feat/alpha").expect("old alpha");
    let old_beta = repo.resolve_commit("feat/beta").expect("old beta");
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let repo = Repo::open(&lab.work).expect("reopen");
    let new_alpha = repo.resolve_commit("feat/alpha").expect("new alpha");
    let new_beta = repo.resolve_commit("feat/beta").expect("new beta");

    // When: only alpha is advanced by name.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");

    // Then: alpha moved, beta did not.
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha),
        "the named member did not reach its tip: {parents:?}"
    );
    assert!(
        !parents.contains(&old_alpha),
        "the named member's old parent stayed: {parents:?}"
    );
    assert!(
        parents.contains(&old_beta),
        "advance moved an unnamed member: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "alpha.txt"),
        "alpha\nmore\n"
    );
    assert!(
        stdout.contains("advanced feat/alpha"),
        "the delta must name what moved: {stdout}"
    );
    let after_named = parents.len();

    // And: a bare advance moves every member that has advanced.
    let output = knives_release(&lab, &home, &["advance"]);
    assert!(output.status.success(), "{output:?}");
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_beta),
        "the bare advance left the advanced member behind: {parents:?}"
    );
    assert!(
        !parents.contains(&old_beta),
        "the bare advance kept the stale parent: {parents:?}"
    );
    assert_eq!(
        parents.len(),
        after_named,
        "a bare advance added or lost a parent: {parents:?}"
    );
    assert!(
        parents.contains(&new_alpha),
        "the bare advance moved the already-advanced member back: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "beta.txt"),
        "beta\nmore\n"
    );
}

#[test]
fn a_named_advance_claims_nothing_about_the_members_it_never_read() {
    // Given: one member already at its tip and one that has advanced. A named
    // advance looked only at what it was given, so "every member is at its
    // branch tip" is a fact it is not in a position to state.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    extend_branch(&lab, "feat/beta", "beta.txt", "beta\nmore\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    // When: only the member that has not moved is named.
    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    // Then: it reports alpha, stays quiet about beta, and moves nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("feat/alpha is already at its tip"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("every member of"),
        "a named advance claimed the sweep only a bare advance performs: {stdout}"
    );
    assert_eq!(release_parent_commits(&lab, "release/2026-08-04"), before);
}

#[test]
fn cut_with_a_previous_release_duplicates_it_verbatim() {
    // Given: a release, and a branch created after it. A cut is a new name for
    // the composition in hand, never a recomputation: the new branch joins
    // through `include`, and nothing advances.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let previous = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let parents = release_parent_commits(&lab, "release/2026-08-05");
    assert_eq!(parents, previous, "a cut must not recompute membership");
    let gamma = commit_at(&lab, "feat/gamma");
    assert!(
        !parents.contains(&gamma),
        "a branch must join through include, not by existing: {parents:?}"
    );
    assert!(stdout.contains("reaped release/2026-08-04"), "{stdout}");
    assert!(
        !Repo::open(&lab.work)
            .expect("reopen after the cut")
            .bookmark_tips()
            .expect("read bookmark tips")
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04"),
        "the reap was reported but the superseded bookmark survived"
    );
}

#[test]
fn a_fixed_scheme_cut_carries_the_local_release_in_hand() {
    // Given: a fixed release branch, published, then edited locally. The cut
    // must name what is here — duplicating the stale published position would
    // silently revert the unpushed include.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let first = knives_release(&lab, &home, &["cut"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first fixed cut")
            .resolve_commit("integration")
            .is_ok(),
        "first fixed cut was not named: {first:?}"
    );
    lab.push_branch("integration");
    lab.fetch_work();
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let included = knives_release(&lab, &home, &["include", "feat/beta"]);
    assert!(
        String::from_utf8_lossy(&included.stdout).contains("included feat/beta"),
        "{included:?}"
    );
    let repo = Repo::open(&lab.work).expect("open after the include");
    let alpha = repo.resolve_commit("feat/alpha").expect("alpha tip");
    let beta = repo.resolve_commit("feat/beta").expect("beta tip");
    let edited = repo
        .resolve_commit("integration")
        .expect("locally edited release tip");

    // When: the fixed branch is cut again.
    let output = knives_release(&lab, &home, &["cut"]);

    // Then: the cut ran, and the composition in hand survived it: a fresh
    // commit whose parents are still both members. This registry lists no
    // consumers, so pinned-ness is unknown and the run is incomplete — which is
    // not the cut refusing, so the assertions below hold it to having cut.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("cut integration as"), "{stdout}");
    let recut = commit_at(&lab, "integration");
    assert_ne!(recut, edited, "the fixed cut named nothing new: {stdout}");
    let parents = release_parent_commits(&lab, "integration");
    assert!(
        parents.contains(&beta),
        "the cut reverted an unpushed include: {parents:?}\n{stdout}"
    );
    assert!(
        parents.contains(&alpha),
        "the cut lost a member it started with: {parents:?}\n{stdout}"
    );
}

#[test]
fn an_edit_refuses_when_the_upstream_trunk_cannot_resolve() {
    // Given: a registry whose base names a branch upstream does not have.
    // Edits classify parents against the trunk; guessing with no trunk would
    // let a drop or advance touch the base, which is rebase's domain.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nbase = \"missing\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write broken-trunk registry");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("cannot resolve")
            && stdout.contains("release edits classify parents against the upstream trunk"),
        "the refusal must name the missing trunk as its reason: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "an edit ran without a resolvable trunk"
    );
}

#[test]
fn a_bare_advance_with_an_ambiguous_parent_changes_nothing() {
    // Given: one released parent with two advanced branches descending from it.
    // A bare advance promises every advanced member; advancing some while
    // skipping the ambiguous one would report success on a composition nobody
    // asked for.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let released = commit_at(&lab, "feat/alpha");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha\nmore\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new", released.as_str(), "-m", "rival follow-up"]);
    std::fs::write(lab.work.join("rival.txt"), "rival\n").expect("write rival");
    lab.jj_work(["bookmark", "create", "feat/rival", "-r", "@"]);
    lab.jj_work(["new"]);
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("nothing advanced"), "{stdout}");
    assert!(
        stdout.contains("several advanced branches")
            && stdout.contains("feat/alpha")
            && stdout.contains("feat/rival"),
        "the refusal must name the branches it could not choose between: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "an ambiguous bare advance mutated the release"
    );
}

#[test]
fn a_drop_without_a_why_is_a_usage_error() {
    // Dropping shipped content without a reason is how a release becomes
    // unexplainable later; the parser refuses rather than defaulting one in.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = knives_release(&lab, &home, &["drop", "feat/alpha"]);

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "{output:?}"
    );
}

#[test]
fn include_by_commit_id_adds_that_exact_parent() {
    // A commit that no bookmark names is still includable by id.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/loose", "loose.txt", "loose\n");
    let loose = commit_at(&lab, "feat/loose");
    lab.jj_work(["bookmark", "forget", "feat/loose"]);
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", loose.as_str()]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    let after = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        after.contains(&loose),
        "the raw commit id was not added as a parent: {after:?}"
    );
    assert_eq!(after.len(), before.len() + 1, "{before:?} -> {after:?}");
    assert_eq!(
        file_at_revision(&lab, "release/2026-08-04", "loose.txt"),
        "loose\n"
    );
}

#[test]
fn include_of_content_reachable_through_another_parent_reports_the_carrier() {
    // Given: beta stacked on alpha, alpha's own parent dropped. Alpha's content
    // still ships through beta's history, but membership is the parent set, so
    // include says which situation holds instead of pretending it happened.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["new", "feat/alpha", "-m", "stacked work"]);
    std::fs::write(lab.work.join("beta.txt"), "beta\n").expect("write beta");
    lab.jj_work(["bookmark", "create", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = home_after_first_cut(&lab);
    let dropped = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "beta carries it"],
    );
    assert!(dropped.status.success(), "{dropped:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    let before_commit = commit_at(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("through another parent's history"),
        "{stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "include mutated the release for content it does not carry"
    );
    assert_eq!(
        commit_at(&lab, "release/2026-08-04"),
        before_commit,
        "a reported non-include rewrote the release"
    );
}

#[test]
fn a_conflicted_cut_defers_reaping_the_resolution_carrier() {
    // Given: two members entangled in one file, so the release is conflicted.
    // A superseded cut is the record of how conflicts were last resolved:
    // reaping it while the successor is unresolved destroys the record exactly
    // when an abandon-and-recut would need it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        Repo::open(&lab.work)
            .expect("open first cut")
            .resolve_commit("release/2026-08-04")
            .is_ok(),
        "first cut was not named: {first:?}"
    );

    // When: a successor is cut while the conflicts stand.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the previous cut survives until the conflicts are resolved.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "kept release/2026-08-04: the live cut release/2026-08-05 still carries conflicts"
        ),
        "no deferral notice: {stdout}"
    );
    assert!(!stdout.contains("reaped release/2026-08-04"), "{stdout}");
    let tips = Repo::open(&lab.work)
        .expect("reopen after conflicted cut")
        .bookmark_tips()
        .expect("read bookmark tips");
    for name in ["release/2026-08-04", "release/2026-08-05"] {
        assert!(
            tips.keys()
                .any(|reference| reference.branch().as_str() == name),
            "{name} missing: {tips:?}"
        );
    }

    // And: an explicit `release reap` obeys the same gate. The cut's own output
    // points at this command, so an agent following it must not be able to
    // destroy the record either.
    let reap = knives_release(&lab, &home, &["reap"]);
    let reap_stdout = String::from_utf8_lossy(&reap.stdout);
    assert!(
        reap_stdout.contains("kept release/2026-08-04"),
        "reap said nothing about the deferral it made: {reap_stdout}"
    );
    assert!(
        !reap_stdout.contains("reaped release/2026-08-04"),
        "{reap_stdout}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after manual reap")
            .bookmark_tips()
            .expect("read bookmark tips")
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04"),
        "a manual reap destroyed the resolution carrier under a conflicted live cut"
    );
}

#[test]
fn a_bare_advance_under_the_fixed_scheme_ignores_the_release_bookmark() {
    // Given: a fixed release branch, whose own bookmark descends from every
    // member parent. A bare advance looks for bookmarks that moved past a
    // parent, so the release is the one bookmark it must never count as one:
    // "advancing" a member onto the release commit would make the release its
    // own member, and under this scheme the name never changes to reveal it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nrelease_branch = \"integration\"\n",
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write fixed-scheme registry");
    let first = knives_release(&lab, &home, &["cut"]);
    let released = Repo::open(&lab.work)
        .expect("open first fixed cut")
        .resolve_commit("integration")
        .unwrap_or_else(|error| panic!("first fixed cut was not named: {error}\n{first:?}"));
    extend_branch(&lab, "feat/alpha", "alpha.txt", "alpha\nmore\n");
    let advanced_alpha = commit_at(&lab, "feat/alpha");

    let output = knives_release(&lab, &home, &["advance"]);

    // Then: the member moved to its branch tip, and the release bookmark was
    // not mistaken for a branch that carries it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("advanced feat/alpha"), "{stdout}");
    let parents = release_parent_commits(&lab, "integration");
    assert!(parents.contains(&advanced_alpha), "{parents:?}");
    assert!(
        !parents.contains(&released),
        "the release bookmark was treated as a member's successor: {parents:?}"
    );
    assert_eq!(
        file_at_revision(&lab, "integration", "alpha.txt"),
        "alpha\nmore\n"
    );
}

#[test]
fn a_member_landed_upstream_by_merge_commit_can_still_be_dropped() {
    // Given: a released member whose pull merged upstream WITH A MERGE COMMIT,
    // so its tip is now reachable from the trunk. Every parent is a member —
    // the base is never one — so landing must not dead-end the post-merge
    // `drop`, and the drop must say the release itself no longer carries it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let alpha = commit_at(&lab, "feat/alpha");
    let beta = commit_at(&lab, "feat/beta");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    assert!(
        Repo::open(&lab.work)
            .expect("open after merge")
            .is_ancestor(&alpha, &commit_at(&lab, "main@upstream"))
            .expect("ancestry answerable"),
        "fixture must land alpha in the trunk by merge commit"
    );

    // When: the landed member is dropped.
    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "merged upstream"],
    );

    // Then: it leaves; the other member stays; and because no remaining member
    // reaches alpha's content, the loss is stated.
    assert!(
        output.status.success(),
        "dropping a landed member dead-ended: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no remaining member carries feat/alpha's content"),
        "{stdout}"
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert!(
        !parents.contains(&alpha),
        "landed alpha survived: {parents:?}"
    );
    assert_eq!(parents, vec![beta], "only beta expected: {parents:?}");
}

#[test]
fn a_drop_whose_content_survives_through_another_member_stays_quiet() {
    // Given: beta stacked on alpha, both members. Dropping alpha loses nothing:
    // beta's ancestry still carries it, and saying otherwise would train people
    // to ignore the loss warning.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["new", "feat/alpha", "-m", "stacked work"]);
    std::fs::write(lab.work.join("beta.txt"), "beta\n").expect("write beta");
    lab.jj_work(["bookmark", "create", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "beta carries it"],
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        !stdout.contains("loses it"),
        "content survives through beta; no loss to report: {stdout}"
    );
    let parents = release_parent_commits(&lab, "release/2026-08-04");
    assert_eq!(parents, vec![commit_at(&lab, "feat/beta")], "{parents:?}");
}

#[test]
fn an_edit_before_any_cut_says_to_cut_one_first() {
    // Given: branches and no release at all. Membership is a release's parent
    // set, so there is nothing to edit yet, and an include that invented a
    // release would ship a composition that never passed the cut's gates.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("no release to edit; cut one first"),
        "{stdout}"
    );
    let tips = Repo::open(&lab.work)
        .expect("open after the refusal")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str().starts_with("release/")),
        "an edit invented a release: {tips:?}"
    );
}

#[test]
fn an_edit_refuses_when_every_pin_of_the_release_is_frozen() {
    // Given: a dated release whose only consumer pins it by revision. Editing
    // it in place reaches nobody, exactly as a rebase would not, so the edit is
    // refused in favour of the one remedy that reaches consumers: a new dated
    // cut. Editing anyway would report a change nothing consumes.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let consumer = lab.consumer_with_pin_history(
        "pyproject.toml",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-03\" }\n",
        "work = { git = \"https://forge.invalid/acme/work.git\", rev = \"release/2026-08-04\" }\n",
    );
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\nconsumers = [\"{}\"]\n",
            lab.work.display(),
            lab.upstream.display(),
            consumer.display(),
        ),
    )
    .expect("write frozen-pin registry");
    let before = release_parent_commits(&lab, release);

    let output = knives_release(&lab, &home, &["include", "feat/gamma"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("frozen") && stdout.contains("cut a new dated release"),
        "frozen-pin guidance missing: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, release),
        before,
        "a release no pin follows was edited in place"
    );
}

#[test]
fn the_audit_catches_a_cut_missing_a_members_content() {
    // Given: a cut that names both feature tips as parents but whose tree loses beta.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");
    lab.jj_work(["bookmark", "create", "doctored-cut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "doctored-cut"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("remove beta from cut tree");
    lab.jj_work(["bookmark", "set", "doctored-cut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("doctored-cut")
        .expect("resolve doctored cut");

    // When: the cut is audited against both captured feature tips.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the omitted branch is the only missing member.
    assert_eq!(audit.missing, vec!["feat/beta".to_owned()], "{audit:?}");
    assert!(audit.unexplained.is_empty(), "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_a_faithful_cut() {
    // Given: two feature tips included in a flat cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };

    // When: the cut is audited against its captured members.
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: every member's content is present.
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn divergence_the_previous_release_already_carried_is_not_a_loss() {
    // Given: a previous release whose recorded resolution dropped beta's file,
    // and a fresh cut duplicated from it — the same published divergence.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-04".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let previous =
        knives::commands::release::build_cut(&lab.work, &request, None).expect("build previous");
    lab.jj_work([
        "bookmark",
        "create",
        "resolved-previous",
        "-r",
        previous.as_str(),
    ]);
    lab.jj_work(["edit", "resolved-previous"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("drop beta by resolution");
    lab.jj_work(["bookmark", "set", "resolved-previous", "-r", "@"]);
    lab.jj_work(["new"]);
    let previous = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("resolved-previous")
        .expect("resolve doctored previous");
    let recut = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        ..request
    };
    let cut = knives::commands::release::build_cut(&lab.work, &recut, Some(&previous))
        .expect("build recut");

    // When: the recut is audited with the previous release in view.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: Some(&previous),
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the carried divergence is reported without failing the audit.
    assert_eq!(audit.carried, vec!["feat/beta".to_owned()], "{audit:?}");
    assert!(audit.missing.is_empty(), "{audit:?}");
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn a_loss_the_previous_release_did_not_have_still_fails_the_audit() {
    // Given: a previous release that faithfully carries beta, and a recut whose
    // tree lost beta's file — new divergence, the incident the audit exists for.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-04".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let previous =
        knives::commands::release::build_cut(&lab.work, &request, None).expect("build previous");
    let recut = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        ..request
    };
    let cut = knives::commands::release::build_cut(&lab.work, &recut, Some(&previous))
        .expect("build recut");
    lab.jj_work(["bookmark", "create", "lossy-recut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "lossy-recut"]);
    std::fs::remove_file(lab.work.join("beta.txt")).expect("lose beta from the recut");
    lab.jj_work(["bookmark", "set", "lossy-recut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("lossy-recut")
        .expect("resolve lossy recut");

    // When: the recut is audited with the previous release in view.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: Some(&previous),
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the new loss fails the audit; nothing about it is "carried".
    assert_eq!(audit.missing, vec!["feat/beta".to_owned()], "{audit:?}");
    assert!(audit.carried.is_empty(), "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_a_faithful_multi_commit_member_without_inconclusive() {
    // Given: alpha has two commits that both touch its original file, and both
    // feature tips are included in a flat cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "first\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "alpha follow-up"]);
    std::fs::write(lab.work.join("alpha.txt"), "first\nsecond\n").expect("extend alpha");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");

    // When: the fresh cut is audited using the captured tips.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: replaying the net member effect sees it as present without a
    // manufactured intermediate-commit conflict.
    assert!(audit.inconclusive.is_empty(), "{audit:?}");
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_when_a_member_adds_then_deletes_a_file() {
    // Given: alpha's final tree has no trace of the file it added in its first commit.
    let lab = Lab::new();
    lab.branch("feat/alpha", "z.txt", "temporary\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "delete temporary file"]);
    std::fs::remove_file(lab.work.join("z.txt")).expect("delete temporary file");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");
    let deleted_path = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            cut.as_str(),
            "root:z.txt",
        ])
        .output()
        .expect("inspect cut tree for deleted path");
    assert!(
        !deleted_path.status.success(),
        "the faithful cut unexpectedly contains z.txt: {}",
        String::from_utf8_lossy(&deleted_path.stderr)
    );

    // When: the faithful cut is audited using alpha's two-commit range.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the net-zero member is present and does not abandon the cut.
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_catches_a_member_whose_early_range_content_is_missing() {
    // Given: alpha's first commit adds early content and its second adds late content.
    let lab = Lab::new();
    lab.branch("feat/alpha", "early.txt", "early\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "add late content"]);
    std::fs::write(lab.work.join("late.txt"), "late\n").expect("write late content");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");
    lab.jj_work(["bookmark", "create", "doctored-cut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "doctored-cut"]);
    std::fs::remove_file(lab.work.join("early.txt")).expect("remove early content from cut");
    lab.jj_work(["bookmark", "set", "doctored-cut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("doctored-cut")
        .expect("resolve doctored cut");

    // When: the doctored cut is audited against alpha's complete captured range.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: content absent from the first commit fails the member's whole-range audit.
    assert_eq!(audit.missing, vec!["feat/alpha".to_owned()], "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_passes_when_a_member_renames_a_file() {
    // Given: alpha's second commit moves its first commit's file to a new path.
    let lab = Lab::new();
    lab.branch("feat/alpha", "old.txt", "content\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.jj_work(["new", "feat/alpha", "-m", "rename feature file"]);
    std::fs::rename(lab.work.join("old.txt"), lab.work.join("new.txt"))
        .expect("rename feature file");
    lab.jj_work(["bookmark", "set", "feat/alpha", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");

    // When: the cut includes the renamed tree and audits the full member range.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the faithful rename is present and leaves the cut auditable.
    assert!(audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_fails_when_a_regenerated_lockfile_loses_member_content() {
    // Given: alpha adds a two-entry lockfile, but the cut's conflict-free tree
    // carries only the regenerated first entry.
    let lab = Lab::new();
    lab.branch("feat/alpha", "uv.lock", "pkg-a\npkg-b\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![trunk.clone(), alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");
    lab.jj_work(["bookmark", "create", "doctored-cut", "-r", cut.as_str()]);
    lab.jj_work(["edit", "doctored-cut"]);
    std::fs::write(lab.work.join("uv.lock"), "pkg-a\n").expect("regenerate lockfile");
    lab.jj_work(["bookmark", "set", "doctored-cut", "-r", "@"]);
    lab.jj_work(["new"]);
    let cut = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("doctored-cut")
        .expect("resolve doctored cut");

    // When: the partially regenerated cut is audited against captured tips.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: the divergent member fails instead of being passed as inconclusive.
    assert_eq!(audit.missing, vec!["feat/alpha".to_owned()], "{audit:?}");
    assert!(!audit.passed(), "{audit:?}");
}

#[test]
fn the_audit_judges_the_captured_tip_not_the_moved_bookmark() {
    // Given: a faithful cut and its captured feature tips.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let trunk = repo.resolve_commit("main@origin").expect("resolve trunk");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut = knives::commands::release::build_cut(&lab.work, &request, None).expect("build cut");
    lab.jj_work(["new", "-r", "feat/beta", "-m", "beta moved after planning"]);
    std::fs::write(lab.work.join("beta-next.txt"), "new beta work\n").expect("write moved beta");
    lab.jj_work(["bookmark", "set", "feat/beta", "-r", "@"]);
    lab.jj_work(["new"]);

    // When: the audit receives the original tip rather than resolving the bookmark.
    let audit = knives::commands::release::audit_cut(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), alpha),
            ("feat/beta".to_owned(), beta),
        ],
        &cut,
        knives::commands::release::AuditContext {
            previous: None,
            trunk: &trunk,
        },
    )
    .expect("audit cut");

    // Then: later work on the bookmark does not make the already faithful cut fail.
    assert!(audit.passed(), "{audit:?}");
}

fn resolved_two_branch_cut(lab: &Lab) -> (CommitId, CommitId, CommitId) {
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let repo = Repo::open(&lab.work).expect("open");
    let alpha = repo.resolve_commit("feat/alpha").expect("resolve alpha");
    let beta = repo.resolve_commit("feat/beta").expect("resolve beta");
    let gamma = repo.resolve_commit("feat/gamma").expect("resolve gamma");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-04".to_owned(),
        parents: vec![alpha.clone(), beta.clone()],
        provenance: vec![
            (alpha.clone(), "feat/alpha".to_owned()),
            (beta.clone(), "feat/beta".to_owned()),
        ],
    };
    let cut =
        knives::commands::release::build_cut(&lab.work, &request, None).expect("build first cut");
    knives::commands::release::name_cut(&lab.work, &request.name, &cut, &ReleaseScheme::Dated)
        .expect("name first cut");
    lab.jj_work(["edit", "release/2026-08-04"]);
    std::fs::write(lab.work.join("shared.txt"), "resolved\n").expect("resolve conflict");
    lab.jj_work(["bookmark", "set", "release/2026-08-04", "-r", "@"]);
    lab.jj_work(["new"]);
    (alpha, beta, gamma)
}

fn file_at_revision(lab: &Lab, revision: &str, file: &str) -> String {
    let output = Command::new("jj")
        .args([
            "--repository",
            lab.work.to_str().expect("utf-8 repository path"),
            "--ignore-working-copy",
            "file",
            "show",
            "-r",
            revision,
            &format!("root:{file}"),
        ])
        .output()
        .expect("show revision file");
    assert!(
        output.status.success(),
        "file show failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("utf-8 file content")
}

/// Run the knives binary's release command for the `demo` repo in `lab`.
fn knives_release(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "release", "--repo", "demo"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run knives release")
}

/// Add a commit to `branch`, leave its bookmark on the new tip, and step off it.
fn extend_branch(lab: &Lab, branch: &str, file: &str, content: &str) {
    lab.jj_work(["new", branch, "-m", "follow-up"]);
    std::fs::write(lab.work.join(file), content).expect("extend a branch");
    lab.jj_work(["bookmark", "set", branch, "-r", "@"]);
    lab.jj_work(["new"]);
}

/// The commit `revision` resolves to right now.
fn commit_at(lab: &Lab, revision: &str) -> CommitId {
    Repo::open(&lab.work)
        .expect("open to resolve a revision")
        .resolve_commit(revision)
        .expect("resolve revision")
}

/// [`release_test_home`], with `release/2026-08-04` already cut from whatever
/// branches `lab` carries: the starting point of every release-edit test.
fn home_after_first_cut(lab: &Lab) -> (tempfile::TempDir, std::path::PathBuf) {
    let (home, consumer) = release_test_home(lab);
    let cut = knives_release(lab, &home, &["cut", "release/2026-08-04"]);
    assert!(cut.status.success(), "{cut:?}");
    (home, consumer)
}

/// The commit each parent of a named release sits at right now.
fn release_parent_commits(lab: &Lab, name: &str) -> Vec<CommitId> {
    Repo::open(&lab.work)
        .expect("open for release parents")
        .parents_of(name)
        .expect("read release parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect()
}

#[test]
fn an_incremental_recut_preserves_the_previous_cuts_conflict_resolutions() {
    // Given: a resolved two-branch cut plus a third branch to include.
    let lab = Lab::new();
    let (alpha, beta, gamma) = resolved_two_branch_cut(&lab);
    let previous = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("release/2026-08-04")
        .expect("resolve previous cut");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha, beta, gamma],
        provenance: Vec::new(),
    };

    // When: the next cut duplicates the resolved cut onto the new parent set.
    let cut = knives::commands::release::build_cut(&lab.work, &request, Some(&previous))
        .expect("build incremental cut");

    // Then: the resolution, new branch content, and new message all survive.
    assert_eq!(
        file_at_revision(&lab, cut.as_str(), "shared.txt"),
        "resolved\n"
    );
    assert_eq!(file_at_revision(&lab, cut.as_str(), "gamma.txt"), "gamma\n");
    assert_eq!(
        lab.revision(&lab.work, cut.as_str(), "description"),
        request.message().trim_end()
    );
    assert!(
        knives::jj::conflicted_files(&lab.work, cut.as_str())
            .expect("list conflicts")
            .is_empty()
    );
}

#[test]
fn a_rebase_preserves_the_previous_releases_conflict_resolution() {
    // Given: two release members whose conflict was resolved by hand in the prior release.
    let lab = Lab::new();
    let _members = resolved_two_branch_cut(&lab);
    let (home, _consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");

    // When: the real binary rebases the release onto the advanced upstream.
    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    // Then: duplicating the old release carries its resolution without a new conflict.
    assert!(
        output.status.success(),
        "rebase failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let release = "release/2026-08-04";
    assert_eq!(file_at_revision(&lab, release, "shared.txt"), "resolved\n");
    assert!(
        knives::jj::conflicted_files(&lab.work, release)
            .expect("list release conflicts")
            .is_empty(),
        "rebase re-created the resolved conflict"
    );
}

#[test]
fn dropping_a_resolved_branch_surfaces_a_focused_conflict_not_silence() {
    // Given: a resolved two-branch cut where beta's content is entangled in the resolution.
    let lab = Lab::new();
    let (alpha, _beta, _gamma) = resolved_two_branch_cut(&lab);
    let previous = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("release/2026-08-04")
        .expect("resolve previous cut");
    let request = knives::commands::release::Cut {
        name: "release/2026-08-05".to_owned(),
        parents: vec![alpha],
        provenance: Vec::new(),
    };

    // When: the next cut drops beta while preserving the prior resolution diff.
    let cut = knives::commands::release::build_cut(&lab.work, &request, Some(&previous))
        .expect("build incremental cut");

    // Then: jj reports the one entangled file as a conflict instead of silently retaining beta.
    assert_eq!(
        knives::jj::conflicted_files(&lab.work, cut.as_str()).expect("list conflicts"),
        vec!["shared.txt".to_owned()]
    );
}

#[test]
fn a_bare_advance_ignores_a_branch_stacked_on_the_release() {
    // Given: a cut, and a branch somebody built on top of it — #4's third loss
    // mode's shape, and the reason `reap` refuses cuts with local descendants.
    // Such a branch descends from every member, so ancestry alone calls it their
    // advanced tip; advancing onto it would fold work nobody included into the
    // release and put the old cut in the new cut's own ancestry.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked on the cut"]);
    std::fs::write(lab.work.join("stacked.txt"), "stacked\n").expect("write stacked content");
    lab.jj_work(["bookmark", "create", "feat/stacked", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains("every member of release/2026-08-04 is at its branch tip"),
        "a branch stacked on the release is not an advanced member: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "the bare advance folded a stacked branch into the release"
    );

    // And: the plan says what the branch actually is, rather than advising the
    // verb that refuses it.
    let plan = knives_release(&lab, &home, &[]);
    let planned = String::from_utf8_lossy(&plan.stdout);
    assert!(
        planned.contains("feat/stacked is stacked on release/2026-08-04"),
        "the plan called a stacked branch an advanced member: {planned}"
    );
}

#[test]
fn advancing_a_named_stacked_branch_is_refused() {
    // Given: the same stacked branch, named outright. Honouring it would make
    // the release contain its own predecessor, so it is answered, not improvised.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked on the cut"]);
    std::fs::write(lab.work.join("stacked.txt"), "stacked\n").expect("write stacked content");
    lab.jj_work(["bookmark", "create", "feat/stacked", "-r", "@"]);
    lab.jj_work(["new"]);

    let output = knives_release(&lab, &home, &["advance", "feat/stacked"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("feat/stacked is stacked on release/2026-08-04"),
        "the refusal must say what is wrong with the branch: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "the refusal still moved a parent"
    );
}

#[test]
fn include_refuses_the_trunk_because_it_is_never_a_member() {
    // Given: a cut, and an upstream trunk that has moved past it. A release is
    // a flat merge of feature and fix branches; upstream enters through the
    // members' bases, never as a parent.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.advance_upstream("upstream advance\n");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    let output = knives_release(&lab, &home, &["include", "main@upstream"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("not a feature or fix branch"),
        "the refusal must state the model: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04"),
        before,
        "including the trunk made it a parent"
    );
}

#[test]
fn an_edit_refuses_a_release_held_only_as_a_remote_ref() {
    // Given: a cut pushed to origin whose local bookmark is gone — the state a
    // fetch of somebody else's cut leaves, because jj creates no local bookmark
    // for an untracked remote one. An edit moves a local bookmark, and jj
    // rejects `name@remote` as a bookmark name, so without a gate the duplicate
    // is made and described before the move fails.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let before = release_parent_commits(&lab, "release/2026-08-04@origin");

    let output = knives_release(&lab, &home, &["include", "feat/beta"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("jj bookmark track release/2026-08-04@origin"),
        "the refusal must say how to get a local bookmark to edit: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04@origin"),
        before,
        "the remote-only release was edited anyway"
    );
}

#[test]
fn a_duplicate_onto_no_parents_is_refused_rather_than_done_in_place() {
    // jj duplicates in place when no destination is given, keeping the source's
    // own parents. A caller that computed an empty parent set would report the
    // change it meant to make while the composition stayed exactly as it was.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let source = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("resolve alpha");

    let error = knives::jj::duplicate_onto(&lab.work, &source, &[]).expect_err("must refuse");

    assert!(
        error.to_string().contains("destination parent"),
        "the error must say what was missing: {error}"
    );
}

#[test]
fn advance_refuses_the_trunk_and_the_release_by_name() {
    // Given: a cut. A bare advance never treats the trunk or a release bookmark
    // as a branch that carries a member, and naming one must not reach the mover
    // it is kept away from: a member advanced onto the trunk gives the release a
    // second base, and one advanced onto a release makes it carry a whole cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let before = release_parent_commits(&lab, "release/2026-08-04");

    for named in ["main", "release/2026-08-04"] {
        let output = knives_release(&lab, &home, &["advance", named]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(3), "{named}: {stdout}");
        assert!(
            stdout.contains("is the trunk or a release name"),
            "{named} must be refused as unadvanceable: {stdout}"
        );
        assert_eq!(
            release_parent_commits(&lab, "release/2026-08-04"),
            before,
            "advancing {named} moved a parent"
        );
    }
}

#[test]
fn a_rebase_refuses_a_release_held_only_as_a_remote_ref() {
    // A rebase moves the release bookmark the same way an edit does, and it does
    // so after the duplicate has been made and described. With no local bookmark
    // to move, that leaves a described commit behind and fails on jj's bookmark
    // name parser instead of saying what is missing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    lab.jj_work(["bookmark", "forget", "release/2026-08-04"]);
    lab.advance_upstream("upstream advance\n");
    let before = release_parent_commits(&lab, "release/2026-08-04@origin");

    let output = knives_release(&lab, &home, &["rebase"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("jj bookmark track release/2026-08-04@origin"),
        "the refusal must say how to get a local bookmark to move: {stdout}"
    );
    assert_eq!(
        release_parent_commits(&lab, "release/2026-08-04@origin"),
        before,
        "the remote-only release was rebased anyway"
    );
}
