//! Which bookmarks keep the work on top of the previous release when the
//! next release is cut.
//!
//! The orphan gate of `knives release cut` lists each commit on top of the
//! previous release that the cut leaves and nothing keeps, and refuses the
//! cut. A branch of `origin` or of the publish remote keeps the work it
//! holds, tracked or not, as a local bookmark does. A branch with a release
//! name keeps nothing, and neither does a branch of `upstream`. The published
//! remedy of `adopt` reads the keepers the same way. These tests drive only
//! the knives binary.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::ids::CommitId;
use knives::jj::Repo;
use lab::{Lab, commit_at, knives_release, release_parents, release_test_home};

/// The previous release, P.
const PREVIOUS: &str = "release/2026-08-04";
/// The cut each test takes after P.
const NEXT: &str = "release/2026-08-05";

/// A fork whose publish remote is a remote of its own, `release`, in the
/// shape of the orphan gate's rehearsal: the previous release P, a flat merge of
/// alpha and beta, pushed to `release` and tagged; and C1, a commit on top of
/// P that the next cut does not carry, as a lane's branch was on 8.0.9.
///
/// Only `holder`, a `(remote, branch)`, holds C1: the branch is pushed there
/// and forgotten here, so it is that remote's alone, and untracked. `None`
/// leaves C1 held by nothing. Returns the lab, its home, P and C1.
fn work_on_a_published_release(
    holder: Option<(&str, &str)>,
) -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let release_remote = lab.temp_path().join("release.git");
    lab::git_bare_repository(&release_remote);
    let release_url = release_remote.to_str().expect("utf-8 path");
    lab.jj_work(["git", "remote", "add", "release", release_url]);
    let (home, _consumer) = release_test_home(&lab);
    let registry = home.path().join("repos.toml");
    let entry = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(&registry, format!("{entry}release = \"{release_url}\"\n"))
        .expect("name the publish remote");
    let cut = knives_release(&lab, &home, &["cut", PREVIOUS]);
    assert!(cut.status.success(), "{cut:?}");
    assert_eq!(
        release_parents(&lab, PREVIOUS).len(),
        2,
        "P must be a flat merge of alpha and beta"
    );
    let previous = commit_at(&lab, PREVIOUS);
    lab.jj_work(["git", "push", "--remote", "release", "--bookmark", PREVIOUS]);
    lab.jj_work(["tag", "set", "v1.0.0", "-r", PREVIOUS]);
    lab.jj_work(["new", PREVIOUS, "-m", "work on the release"]);
    std::fs::write(lab.work.join("n1.txt"), "n1\n").expect("write C1");
    lab.jj_work(["new"]);
    let on_top = CommitId::new(lab.revision(&lab.work, "@-", "commit_id"));
    lab.jj_work(["new", "main@origin"]);
    if let Some((remote, branch)) = holder {
        lab.jj_work(["bookmark", "create", branch, "-r", on_top.as_str()]);
        lab.jj_work(["git", "push", "--remote", remote, "--bookmark", branch]);
        lab.jj_work(["bookmark", "forget", branch]);
    }
    let held_by: Vec<String> = Repo::open(&lab.work)
        .expect("open")
        .bookmark_tips()
        .expect("read bookmark tips")
        .into_iter()
        .filter(|(_, commit)| *commit == on_top)
        .map(|(reference, _)| reference.to_string())
        .collect();
    let named: Vec<String> = holder
        .map(|(remote, branch)| format!("{branch}@{remote}"))
        .into_iter()
        .collect();
    assert_eq!(
        held_by, named,
        "C1 must be held by the named branch alone, or this test proves nothing"
    );
    (lab, home, previous, on_top)
}

/// Assert that the cut in `output` was made and that its orphan gate did not
/// refuse it or list `commit`.
fn assert_not_refused_for(output: &std::process::Output, commit: &CommitId) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let listed = format!("    {}", commit.short());
    assert!(
        !stdout.contains("would be dropped") && !stdout.lines().any(|line| line == listed),
        "the orphan gate listed {}: {stdout}",
        commit.short()
    );
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains(&format!("cut {NEXT} as ")), "{stdout}");
}

/// Assert that the orphan gate refused the cut in `output` (exit 3), listing
/// `commit` alone, and that nothing was cut.
fn assert_refused_for(lab: &Lab, output: &std::process::Output, commit: &CommitId) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "demo: refusing to cut: 1 commit(s) are reachable only from {PREVIOUS} or its \
             descendants and would be dropped:\n    {}\n",
            commit.short()
        )),
        "{stdout}"
    );
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after the refusal")
            .resolve_commit(NEXT)
            .is_err(),
        "the refused cut was made anyway"
    );
}

#[test]
fn the_orphan_gate_keeps_a_commit_an_untracked_origin_branch_holds() {
    // Given (R10, K13 1): C1 on top of the published P, held only by an
    // untracked branch of `origin`, as BET's branches of sjawhar/rocq held
    // work on 8.0.9.
    let (lab, home, _previous, on_top) = work_on_a_published_release(Some(("origin", "bet/n1")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate keeps C1, and the cut is made.
    assert_not_refused_for(&output, &on_top);
}

#[test]
fn the_orphan_gate_keeps_a_commit_a_publish_remote_branch_holds() {
    // Given (R10, K13 2): C1 held only by a branch of the publish remote,
    // under a name that is not a release name.
    let (lab, home, _previous, on_top) = work_on_a_published_release(Some(("release", "bet/n1")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate keeps C1, and the cut is made.
    assert_not_refused_for(&output, &on_top);
}

#[test]
fn the_orphan_gate_refuses_a_commit_nothing_holds() {
    // Given (R10, K13 3, a control): C1 held by nothing.
    let (lab, home, _previous, on_top) = work_on_a_published_release(None);

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate refuses it and lists C1.
    assert_refused_for(&lab, &output, &on_top);
}

#[test]
fn a_release_named_origin_branch_keeps_nothing() {
    // Given (R10, K13 4, a control): C1 held only by `release/scratch` on
    // `origin`, a release name that is not a dated cut.
    let (lab, home, _previous, on_top) =
        work_on_a_published_release(Some(("origin", "release/scratch")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate refuses it and lists C1.
    assert_refused_for(&lab, &output, &on_top);
}

#[test]
fn an_upstream_branch_keeps_nothing() {
    // Given (R10, K13 5, a control): C1 held only by a branch of `upstream`,
    // which its owner can delete.
    let (lab, home, _previous, on_top) = work_on_a_published_release(Some(("upstream", "bet/n1")));

    // When: the next release is cut.
    let output = knives_release(&lab, &home, &["cut", NEXT]);

    // Then: the orphan gate refuses it and lists C1.
    assert_refused_for(&lab, &output, &on_top);
}

/// Edit the published P by hand: a new commit on P's parents with P's tree
/// and alpha repaired, the name moved onto it. The tag makes P immutable to
/// jj, so P itself cannot be edited. Returns the edit.
fn edit_the_published_release(lab: &Lab) -> CommitId {
    let parents = release_parents(lab, PREVIOUS);
    lab.jj_work([
        "new",
        parents[0].as_str(),
        parents[1].as_str(),
        "-m",
        PREVIOUS,
    ]);
    lab.jj_work(["restore", "--from", PREVIOUS]);
    std::fs::write(lab.work.join("alpha.txt"), "alpha, repaired\n").expect("repair alpha");
    lab.jj_work(["bookmark", "set", PREVIOUS, "-r", "@", "--allow-backwards"]);
    lab.jj_work(["new", "main@origin"]);
    commit_at(lab, PREVIOUS)
}

/// The line of the published remedy that `adopt` prints for `edited`, the
/// hand edit of P (`previous`). Its cut states the drop of `dropped`, where
/// the orphan gate would list that commit.
fn published_remedy(previous: &CommitId, edited: &CommitId, dropped: Option<&CommitId>) -> String {
    let stated = dropped.map_or_else(String::new, |commit| {
        format!(
            " with `--allow-drop {}`, which must print `--allow-drop: nothing dropped` naming \
             exactly {} (any other answer is a stop)",
            commit.short(),
            commit.short()
        )
    });
    format!(
        "demo: {PREVIOUS} is published (tag v1.0.0 names {p}), so a hand edit cannot be \
         recorded under it: move {PREVIOUS} back to its published commit ({PREVIOUS}@release, \
         {p}) (`jj bookmark set {PREVIOUS} -r {p} --allow-backwards`), then if the working copy \
         is on {e} or on an empty commit on it, `jj new 'trunk()'`, then cut a new dated \
         name{stated}, then `jj restore --from {e} --into <new name>`, then adopt the new name, \
         then `jj abandon {e}`, then `jj bookmark forget {PREVIOUS}` (never `jj bookmark \
         delete`, which a push would carry to the remote)\n",
        p = previous.short(),
        e = edited.short(),
    )
}

#[test]
fn the_published_remedy_drops_no_commit_an_origin_branch_holds() {
    // Given (R10, K13 6): the published P edited by hand, and C1 held only
    // by an untracked branch of `origin`.
    let (lab, home, previous, on_top) = work_on_a_published_release(Some(("origin", "bet/n1")));
    let edited = edit_the_published_release(&lab);

    // When: the edit is adopted.
    let adopted = knives_release(&lab, &home, &["adopt", "--why", "a repair"]);

    // Then: adopt refuses it with the published remedy, whose cut states no
    // drop: the orphan gate keeps C1.
    let stdout = String::from_utf8_lossy(&adopted.stdout);
    assert_eq!(adopted.status.code(), Some(3), "{stdout}");
    assert!(
        !stdout.contains(on_top.short()),
        "the remedy names {}: {stdout}",
        on_top.short()
    );
    assert!(
        stdout.contains(&published_remedy(&previous, &edited, None)),
        "{stdout}"
    );
}

#[test]
fn the_published_remedy_names_a_commit_nothing_holds() {
    // Given (R10, K13 6, a control): the published P edited by hand, and C1
    // held by nothing.
    let (lab, home, previous, on_top) = work_on_a_published_release(None);
    let edited = edit_the_published_release(&lab);

    // When: the edit is adopted.
    let adopted = knives_release(&lab, &home, &["adopt", "--why", "a repair"]);

    // Then: adopt refuses it with the published remedy, whose cut states the
    // drop of C1.
    let stdout = String::from_utf8_lossy(&adopted.stdout);
    assert_eq!(adopted.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains(&published_remedy(&previous, &edited, Some(&on_top))),
        "{stdout}"
    );
}
