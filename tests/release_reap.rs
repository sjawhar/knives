//! `knives release reap`: superseded dated cuts disappear, and what keeps one.
//!
//! The newest cut stays. A superseded one goes unless a workspace's working
//! copy or other local descendant rests on it, an untracked remote pin or a
//! local bookmark of another name holds it, or it carries the resolution of a
//! conflicted live cut, each said plainly and none stopping the reap of later
//! names; a forgotten and abandoned release stays on the remote. One operation
//! in the op log, and a ref the next fetch rematerialises is cleared again.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::BookmarkRef;
use knives::jj::Repo;
use lab::{
    Lab, commit_at, knives_release, newest_operation_description, operation_ids, release_test_home,
};
use std::process::Command;

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

    // When: the release is reaped in the load-bearing order, then the chained
    // features in one batch.
    let outcome = knives::jj::forget_and_abandon(
        &lab.work,
        &[("release/2026-08-04".to_owned(), vec![release.clone()])],
        "knives: reap release/2026-08-04",
    )
    .expect("reap the release");
    assert!(outcome.refused.is_empty(), "release abandon refused");
    let outcome = knives::jj::forget_and_abandon(
        &lab.work,
        &[
            ("feat/alpha".to_owned(), vec![alpha.clone()]),
            ("feat/alpha-child".to_owned(), vec![alpha_child.clone()]),
            ("feat/beta".to_owned(), vec![beta.clone()]),
        ],
        "knives: reap the feature chain",
    )
    .expect("reap the feature chain");
    assert!(outcome.refused.is_empty(), "feature abandon refused");

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
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

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
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

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
fn release_reap_json_prints_one_json_document() {
    // Given: one superseded cut a stacked commit keeps, one that goes.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-03", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "stacked work"]);
    lab.jj_work(["new", "main@origin"]);
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "main@origin"]);
    let (home, _consumer) = release_test_home(&lab);

    // When: reaped with `--json`.
    let output = lab::release_command(&lab, &home, lab::ReleaseOutput::Json, &["reap"])
        .output()
        .expect("run knives release reap --json");

    // Then: stdout is one JSON document naming both outcomes, and the exit is
    // the kept cut's finding.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("{error}: {stdout}"));
    assert_eq!(report["repo"], "demo", "{report}");
    assert_eq!(
        report["reaped"],
        serde_json::json!(["release/2026-08-03"]),
        "{report}"
    );
    assert_eq!(report["kept"][0]["name"], "release/2026-08-04", "{report}");
    assert!(
        report["kept"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.starts_with("it has descendants")),
        "{report}"
    );
    assert_eq!(output.status.code(), Some(1), "{stdout}");
}

#[test]
fn release_reap_keeps_a_commit_an_untracked_remote_pin_holds_and_exits_clean() {
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
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();
    let (home, _consumer) = release_test_home(&lab);

    // When: standalone reap attempts to remove the superseded cut.
    let output = knives_release(&lab, &home, &["reap"]);

    // Then: the kept commit is reported as what happened - refs gone, commit
    // still pinned - on one line that does not read as an error, and the run
    // exits clean: nothing is left to act on, and a cut's exit is its reap's.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .contains("demo: reaped release/2026-08-04 (refs forgotten everywhere; commit kept, ")
            && stdout.contains("still pinned by"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("refused") && !stdout.contains("immutable"),
        "a pinned commit is not an error: {stdout}"
    );
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_second_workspaces_parked_working_copy_keeps_the_superseded_cut() {
    // Given: another workspace parked (empty, undescribed) on the superseded
    // cut — knives' normal multi-workspace state. jj only auto-discards the
    // CURRENT workspace's @ when it moves; a second workspace's stays.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let cut = commit_at(&lab, "release/2026-08-04");
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
    let parked = lab.revision(&lab.work, "parked@", "commit_id");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

    // Then: the parked working copy keeps the cut, its refs and itself as they
    // were, and the reason names the workspace. Abandoning the cut would
    // rebase that working copy onto the merge's parents from outside its
    // workspace.
    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(report.kept.len(), 1, "{report:?}");
    assert_eq!(report.kept[0].0, "release/2026-08-04", "{report:?}");
    assert!(
        report.kept[0].1.contains("the working copy of parked"),
        "{report:?}"
    );
    assert!(is_visible(&lab, &cut), "the parked cut was abandoned");
    assert_eq!(
        lab.revision(&lab.work, "parked@", "commit_id"),
        parked,
        "the parked working copy was rewritten"
    );
    assert_eq!(commit_at(&lab, "release/2026-08-04"), cut);
}

#[test]
fn a_cut_keeps_the_superseded_cut_another_workspaces_empty_working_copy_sits_on() {
    // Given: a pushed cut, and a second workspace whose working copy is an
    // empty, undescribed child of it: what `knives start` leaves for the agent
    // holding that release. An include then moves the name to a new commit and
    // keeps the pushed one for that working copy. The cutting workspace
    // stands elsewhere.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    lab.push_branch("release/2026-08-04");
    let cut = commit_at(&lab, "release/2026-08-04");
    let other_workspace = lab.temp_path().join("other-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "other",
        "--revision",
        "release/2026-08-04",
        other_workspace.to_str().expect("utf-8 path"),
    ]);
    let parked = lab.revision(&lab.work, "other@", "commit_id");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{included:?}");
    assert!(
        is_visible(&lab, &cut),
        "the include abandoned the cut under the other working copy"
    );
    assert_ne!(commit_at(&lab, "release/2026-08-04"), cut);
    assert_eq!(commit_at(&lab, "release/2026-08-04@origin"), cut);

    // When: the next cut reaps the superseded name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the cut is made; the superseded cut stays, with its refs and the
    // other workspace's working copy on it, and the reap says why and exits 1.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as "),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "demo: kept release/2026-08-04: it has descendants, whose heads are {}, the working copy of other\n",
            knives::ids::CommitId::new(parked.as_str()).short()
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("reaped release/2026-08-04"), "{stdout}");
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        is_visible(&lab, &cut),
        "the cut under another workspace's working copy was abandoned"
    );
    assert_eq!(
        lab.revision(&lab.work, "other@", "commit_id"),
        parked,
        "another workspace's working copy was rewritten"
    );
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("tips");
    assert!(
        tips.keys().any(|reference| matches!(
            reference,
            BookmarkRef::Remote { branch, remote }
                if branch.as_str() == "release/2026-08-04" && remote.as_str() == "origin"
        )),
        "the kept cut's tracking ref was forgotten: {tips:?}"
    );
}

#[test]
fn a_cut_keeps_the_superseded_cut_its_own_working_copy_sits_on_until_that_moves() {
    // Given: the cutting workspace's own working copy parked, empty, on the
    // release in hand, where `knives start` puts the agent holding it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.jj_work(["new", "release/2026-08-04"]);
    let parked = lab.revision(&lab.work, "@", "commit_id");

    // When: the next cut runs from that workspace.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the superseded cut stays under the working copy, which jj still
    // finds fresh, and the reap says why and exits 1.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("cut release/2026-08-05 as ")
            && stdout.contains(&format!(
                "demo: kept release/2026-08-04: it has descendants, whose heads are {}, the working copy of default\n",
                knives::ids::CommitId::new(parked.as_str()).short()
            )),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        is_visible(&lab, &cut),
        "the cut under the working copy was abandoned"
    );
    assert_eq!(lab.revision(&lab.work, "@", "commit_id"), parked);
    lab.jj_work(["status"]);

    // And: once the working copy moves onto the new cut, the reap takes the
    // old one.
    lab.jj_work(["new", "release/2026-08-05"]);
    let reap = knives_release(&lab, &home, &["reap"]);
    let reap_stdout = String::from_utf8_lossy(&reap.stdout);
    assert!(reap.status.success(), "{reap_stdout}");
    assert!(
        reap_stdout.contains("reaped release/2026-08-04"),
        "{reap_stdout}"
    );
    assert!(!is_visible(&lab, &cut), "the reaped cut stayed visible");
}

#[test]
fn a_superseded_cut_a_workspace_has_checked_out_is_kept() {
    // Given: a second workspace editing the superseded cut itself (`jj edit`),
    // so the cut is its working-copy commit rather than the parent of one.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let cut = commit_at(&lab, "release/2026-08-04");
    let editing_workspace = lab.temp_path().join("editing-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "editing",
        "--revision",
        "release/2026-08-04",
        editing_workspace.to_str().expect("utf-8 path"),
    ]);
    lab::jj(&editing_workspace, ["edit", "release/2026-08-04"]);
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

    // Then: the cut stays as that workspace's working copy, and the reason
    // names it.
    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(
        report.kept,
        vec![(
            "release/2026-08-04".to_owned(),
            "it is the working copy of editing".to_owned()
        )],
        "{report:?}"
    );
    assert!(is_visible(&lab, &cut), "the checked-out cut was abandoned");
    assert_eq!(
        lab.revision(&lab.work, "editing@", "commit_id"),
        cut.as_str()
    );
}

#[test]
fn two_superseded_names_on_one_commit_are_reaped_together() {
    // Given: two superseded dated names on one commit. Both are forgotten in
    // the reap's one operation, so neither's bookmark pins the commit against
    // the other's abandon.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work([
        "bookmark",
        "create",
        "release/2026-08-04.1",
        "-r",
        "release/2026-08-04",
    ]);
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

    // Then: both names are reaped and the commit is gone.
    assert_eq!(
        report.reaped,
        vec![
            "release/2026-08-04".to_owned(),
            "release/2026-08-04.1".to_owned()
        ],
        "{report:?}"
    );
    assert!(
        report.forgotten_only.is_empty() && report.kept.is_empty() && report.notes.is_empty(),
        "{report:?}"
    );
    assert!(!is_visible(&lab, &cut), "the reaped cut stayed visible");
}

#[test]
fn a_cut_keeps_a_superseded_cut_a_retention_bookmark_names_and_exits_clean() {
    // Given: a cut knives wrote, so its ledger records the commit as an earlier
    // write of the name, and a retention bookmark on it.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let cut = commit_at(&lab, "release/2026-08-04");
    lab.jj_work([
        "bookmark",
        "create",
        "keep/2026-08-04",
        "-r",
        "release/2026-08-04",
    ]);

    // When: the next cut reaps the superseded name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is forgotten and the commit stays under the bookmark,
    // untouched by the cleanup of the name's earlier writes; the run exits 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "demo: reaped release/2026-08-04 (refs forgotten everywhere; commit kept, {} still pinned by bookmark keep/2026-08-04)\n",
            cut.short()
        )),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("an earlier release/2026-08-04 commit"),
        "{stdout}"
    );
    assert!(output.status.success(), "{stdout}");
    assert!(is_visible(&lab, &cut), "the bookmarked cut was abandoned");
    assert_eq!(commit_at(&lab, "keep/2026-08-04"), cut);
}

#[test]
fn a_cut_keeps_a_superseded_name_whose_earlier_write_a_working_copy_rests_on() {
    // Given: an unpushed cut, a second workspace parked on it, and an include
    // that writes a new commit under the name and keeps the first one for that
    // working copy. No ref names the first commit any more; the ledger does.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let first_write = commit_at(&lab, "release/2026-08-04");
    let other_workspace = lab.temp_path().join("other-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "other",
        "--revision",
        "release/2026-08-04",
        other_workspace.to_str().expect("utf-8 path"),
    ]);
    let parked = lab.revision(&lab.work, "other@", "commit_id");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{included:?}");
    assert_ne!(commit_at(&lab, "release/2026-08-04"), first_write);
    assert!(is_visible(&lab, &first_write), "the include abandoned it");

    // When: the next cut reaps the superseded name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is kept, the reason names the earlier write and the
    // working copy, the run exits 1, and the working copy is untouched.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "demo: kept release/2026-08-04: {}, an earlier release/2026-08-04 commit: it has descendants, whose heads are {}, the working copy of other\n",
            first_write.short(),
            knives::ids::CommitId::new(parked.as_str()).short()
        )),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        is_visible(&lab, &first_write),
        "the earlier write was abandoned"
    );
    assert_eq!(lab.revision(&lab.work, "other@", "commit_id"), parked);

    // And: once the workspace is forgotten, the reap retires the name and both
    // of its commits.
    lab.jj_work(["workspace", "forget", "other"]);
    let reap = knives_release(&lab, &home, &["reap"]);
    let reap_stdout = String::from_utf8_lossy(&reap.stdout);
    assert!(reap.status.success(), "{reap_stdout}");
    assert!(
        reap_stdout.contains(
            "demo: reaped release/2026-08-04 (refs forgotten everywhere, commit abandoned; remote untouched)\n"
        ) && reap_stdout.contains(&format!(
            "demo: abandoned {}, an earlier release/2026-08-04 commit nothing rests on any more\n",
            first_write.short()
        )),
        "{reap_stdout}"
    );
    assert!(
        !is_visible(&lab, &first_write),
        "the earlier write stayed visible"
    );
}

#[test]
fn a_cut_keeps_a_superseded_name_whose_rewritten_earlier_write_a_working_copy_rests_on() {
    // Given: an earlier write of the name kept for another workspace's working
    // copy, then rewritten (described), so the ledger's id is hidden and the
    // working copy rests on the rewrite.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(first.status.success(), "{first:?}");
    let first_write = commit_at(&lab, "release/2026-08-04");
    let other_workspace = lab.temp_path().join("other-ws");
    lab.jj_work([
        "workspace",
        "add",
        "--name",
        "other",
        "--revision",
        "release/2026-08-04",
        other_workspace.to_str().expect("utf-8 path"),
    ]);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(included.status.success(), "{included:?}");
    lab.jj_work([
        "describe",
        "-r",
        first_write.as_str(),
        "-m",
        "release: rewritten by hand",
    ]);
    let rewrite = knives::ids::CommitId::new(lab.revision(&lab.work, "other@-", "commit_id"));
    assert_ne!(rewrite, first_write, "the describe did not rewrite it");
    let parked = lab.revision(&lab.work, "other@", "commit_id");

    // When: the next cut reaps the superseded name.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-05"]);

    // Then: the name is kept for the rewrite, and the working copy is untouched.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "demo: kept release/2026-08-04: {}, an earlier release/2026-08-04 commit: ",
            rewrite.short()
        )),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(is_visible(&lab, &rewrite), "the rewrite was abandoned");
    assert_eq!(lab.revision(&lab.work, "other@", "commit_id"), parked);
}

/// Whether `commit` is visible in the work checkout. Naming a hidden commit in
/// a revset resurrects it, so visibility is membership of `all()`.
fn is_visible(lab: &Lab, commit: &knives::ids::CommitId) -> bool {
    knives::jj::commits_matching(&lab.work, "all()")
        .expect("list visible commits")
        .contains(commit)
}

#[test]
fn reap_reaps_when_another_remote_bookmark_pins_the_cut() {
    // Given: a non-dated origin bookmark created and pushed from work itself,
    // then deleted locally so only its remote ref names the cut. Fetch returns
    // this pin as TRACKED, so it is the mutable contrast to the untracked-pin
    // sibling test; a local bookmark of another name keeps the commit (the
    // local-bookmark sibling test).
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
    lab.jj_work(["bookmark", "delete", "keep/pin"]);
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
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

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
fn a_local_bookmark_of_another_name_keeps_the_commit_and_stays_where_it_was() {
    // Given: a retention bookmark on the superseded cut, made locally. jj's
    // default rule does not pin a local bookmark, and abandoning the cut would
    // move the bookmark onto the merge's parents, one conflict side each.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    lab.jj_work(["bookmark", "create", "keep/pin", "-r", "release/2026-08-04"]);
    let cut = commit_at(&lab, "release/2026-08-04");

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

    // Then: the release name is forgotten, the commit stays under the
    // bookmark, which still names it alone, and the reason names it.
    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(
        report.forgotten_only,
        vec![(
            "release/2026-08-04".to_owned(),
            format!("{} still pinned by bookmark keep/pin", cut.short())
        )],
        "{report:?}"
    );
    assert!(report.notes.is_empty(), "{report:?}");
    assert!(is_visible(&lab, &cut), "the bookmarked cut was abandoned");
    let tips = Repo::open(&lab.work)
        .expect("reopen")
        .bookmark_tips()
        .expect("remaining tips");
    assert_eq!(
        tips.get(&BookmarkRef::Local(knives::ids::BranchName::new(
            "keep/pin"
        ))),
        Some(&cut),
        "{tips:?}"
    );
    assert!(
        !tips
            .keys()
            .any(|reference| reference.branch().as_str() == "release/2026-08-04"),
        "{tips:?}"
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
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

    // Then: refs are forgotten, the commit is kept with its pin named, reaped
    // does not overstate, and a pinned commit is not a note-worthy failure.
    assert!(report.reaped.is_empty(), "{report:?}");
    assert_eq!(report.forgotten_only.len(), 1, "{report:?}");
    let (name, why) = &report.forgotten_only[0];
    assert_eq!(name, "release/2026-08-04");
    assert!(why.contains("still pinned by"), "{report:?}");
    assert!(report.notes.is_empty(), "{report:?}");
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
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .status()
        .expect("push pin from second clone");
    assert!(status.success(), "push pin from second clone");
    lab.fetch_work();

    // When: the workspace is reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");

    // Then: the pinned first name is kept without stopping the second.
    assert_eq!(report.forgotten_only.len(), 1, "{report:?}");
    assert_eq!(report.forgotten_only[0].0, "release/2026-08-04");
    assert!(
        report.forgotten_only[0].1.contains("still pinned by"),
        "{report:?}"
    );
    assert_eq!(report.reaped, vec!["release/2026-08-05".to_owned()]);
}

#[test]
fn reaping_is_one_operation_described_for_the_op_log() {
    // Given: one superseded cut. Reaping used to be two operations per name
    // (bookmark forget, then abandon), each described as raw `args: jj ...`.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.octopus("release/2026-08-05", "feat/alpha", "feat/beta");
    let operations_before = operation_ids(&lab.work);

    // When: reaped.
    let repo = Repo::open(&lab.work).expect("open");
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("reap");
    assert_eq!(report.reaped, vec!["release/2026-08-04".to_owned()]);

    // Then: the whole reap is ONE operation, described as knives' own act.
    let operations_after = operation_ids(&lab.work);
    assert_eq!(
        operations_after.len(),
        operations_before.len() + 1,
        "a reap must be one operation"
    );
    assert_eq!(
        newest_operation_description(&lab.work),
        "knives: reap release/2026-08-04"
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
    knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("first reap");
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
    let report = knives::commands::release::reap_superseded(
        &repo,
        &knives::ids::ReleaseScheme::Dated,
        "origin",
        &[],
    )
    .expect("second reap");

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
