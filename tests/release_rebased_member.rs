//! A member branch rebased onto a newer upstream trunk is still that member.
//!
//! `jj rebase` rewrites the branch's commits, so the released parent stops being
//! an ancestor of the branch; its change id stays on the branch. Every edit that
//! matches a member to its branch has to see that, or the branch reads as a
//! stranger to its own release — which is how agents were led to keep a second
//! copy of every pull request branch on the old base.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
#[allow(
    dead_code,
    reason = "a shared fixture; not every test file uses every helper"
)]
mod lab;

use knives::ids::CommitId;
use knives::jj::Repo;
use lab::{Lab, knives_release, release_test_home};

fn commit_at(lab: &Lab, revision: &str) -> CommitId {
    Repo::open(lab.work_path())
        .expect("open to resolve a revision")
        .resolve_commit(revision)
        .expect("resolve revision")
}

fn release_parents(lab: &Lab, name: &str) -> Vec<CommitId> {
    Repo::open(lab.work_path())
        .expect("open for release parents")
        .parents_of(name)
        .expect("release parents")
        .into_iter()
        .map(|parent| parent.commit)
        .collect()
}

/// A release cut from two members forked from the old trunk, then alpha rebased
/// onto the advanced upstream — the shape a maintainer's "please rebase"
/// produces when the rebase arrives from elsewhere: jj kept alpha's change id,
/// the release still holds the pre-rebase commit, and nothing connects them by
/// ancestry. Built in one repository by rebasing and then pointing the release
/// back at its previous merge, which leaves exactly that state. The cut's ledger
/// record is removed so only ancestry and change ids can answer.
fn rebased_alpha() -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&cut.stdout),
        String::from_utf8_lossy(&cut.stderr)
    );
    std::fs::remove_dir_all(home.path().join("ledger")).expect("forget the cut record");
    let old_alpha = commit_at(&lab, "feat/alpha");
    let old_release = commit_at(&lab, "release/2026-08-04");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["rebase", "-b", "feat/alpha", "-d", "main@upstream"]);
    lab.jj_work([
        "bookmark",
        "set",
        "release/2026-08-04",
        "--allow-backwards",
        "-r",
        old_release.as_str(),
    ]);
    let new_alpha = commit_at(&lab, "feat/alpha");
    assert_ne!(old_alpha, new_alpha, "the rebase must rewrite alpha");
    let repo = Repo::open(lab.work_path()).expect("open");
    assert!(
        !repo.is_ancestor(&old_alpha, &new_alpha).expect("ancestry"),
        "a rebased branch must not descend from its old commit, or this test proves nothing"
    );
    assert!(
        release_parents(&lab, "release/2026-08-04").contains(&old_alpha),
        "the release must still hold the pre-rebase alpha"
    );
    (lab, home, old_alpha, new_alpha)
}

#[test]
fn advance_follows_a_member_rebased_onto_a_newer_trunk() {
    let (lab, home, old_alpha, new_alpha) = rebased_alpha();
    let before = release_parents(&lab, "release/2026-08-04").len();

    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    assert!(
        output.status.success(),
        "advance refused a rebased member: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha),
        "alpha not advanced: {parents:?}"
    );
    assert!(
        !parents.contains(&old_alpha),
        "old alpha survived: {parents:?}"
    );
    assert_eq!(
        parents.len(),
        before,
        "the member count must not change: {parents:?}"
    );
}

#[test]
fn a_bare_advance_moves_the_rebased_member_too() {
    let (lab, home, old_alpha, new_alpha) = rebased_alpha();

    let output = knives_release(&lab, &home, &["advance"]);

    assert!(
        output.status.success(),
        "bare advance refused: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha) && !parents.contains(&old_alpha),
        "{parents:?}"
    );
}

#[test]
fn include_does_not_carry_a_rebased_member_twice() {
    let (lab, home, old_alpha, _new_alpha) = rebased_alpha();
    let before = release_parents(&lab, "release/2026-08-04").len();

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("knives release advance feat/alpha"),
        "include must point at advance for a member that moved on: {stdout}"
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(
        parents.len(),
        before,
        "include added a second copy: {parents:?}"
    );
    assert!(parents.contains(&old_alpha));
}

#[test]
fn the_plan_reports_a_rebased_member_as_moved_on_not_missing() {
    let (lab, home, _old_alpha, _new_alpha) = rebased_alpha();

    let output = knives_release(&lab, &home, &[]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("feat/alpha is not in release/2026-08-04"),
        "a rebased member read as absent: {stdout}"
    );
    assert!(
        stdout.contains("knives release advance feat/alpha"),
        "the plan must say advance follows the rebased branch: {stdout}"
    );
    // Forking from a newer trunk raises no per-branch finding: the only `!!`
    // lines a rebased member leaves are the stale parent and, when the release
    // lags, the trunk. No `!! branch …` line names alpha or beta.
    assert!(
        !stdout
            .lines()
            .any(|line| line.trim_start().starts_with("!! branch ")),
        "forking from a newer trunk is not a finding: {stdout}"
    );
}

#[test]
fn drop_resolves_a_rebased_member_by_its_change() {
    let (lab, home, old_alpha, _new_alpha) = rebased_alpha();
    let before = release_parents(&lab, "release/2026-08-04").len();

    let output = knives_release(
        &lab,
        &home,
        &["drop", "feat/alpha", "--why", "no longer wanted"],
    );

    assert!(
        output.status.success(),
        "drop could not find the rebased member: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        !parents.contains(&old_alpha),
        "alpha survived the drop: {parents:?}"
    );
    assert_eq!(parents.len(), before - 1);
}

/// A rebase done outside jj: new commits, new change ids, nothing in the
/// repository tying them to the released parent. `jj duplicate` onto the
/// advanced trunk produces exactly that.
fn alpha_rebuilt_outside_jj() -> (Lab, tempfile::TempDir, CommitId, CommitId) {
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "cut failed: {}\n{}",
        String::from_utf8_lossy(&cut.stdout),
        String::from_utf8_lossy(&cut.stderr)
    );
    let old_alpha = commit_at(&lab, "feat/alpha");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["duplicate", "feat/alpha", "-d", "main@upstream"]);
    let new_alpha = CommitId::new(
        lab.revision(lab.work_path(), "children(main@upstream)", "commit_id")
            .trim(),
    );
    lab.jj_work([
        "bookmark",
        "set",
        "feat/alpha",
        "--allow-backwards",
        "-r",
        new_alpha.as_str(),
    ]);
    assert_ne!(old_alpha, new_alpha);
    (lab, home, old_alpha, new_alpha)
}

#[test]
fn advance_finds_a_member_rebuilt_outside_jj_through_the_cut_record() {
    let (lab, home, old_alpha, new_alpha) = alpha_rebuilt_outside_jj();
    let before = release_parents(&lab, "release/2026-08-04").len();

    let output = knives_release(&lab, &home, &["advance", "feat/alpha"]);

    assert!(
        output.status.success(),
        "advance refused a member the cut record names: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_alpha) && !parents.contains(&old_alpha),
        "{parents:?}"
    );
    assert_eq!(parents.len(), before);
}

#[test]
fn include_refuses_a_second_copy_of_a_member_the_cut_record_names() {
    let (lab, home, old_alpha, _new_alpha) = alpha_rebuilt_outside_jj();
    let before = release_parents(&lab, "release/2026-08-04").len();

    let output = knives_release(&lab, &home, &["include", "feat/alpha"]);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would carry it twice")
            && stdout.contains(&format!(
                "as {} per its last cut",
                &old_alpha.as_str()[..12]
            ))
            && stdout.contains("`knives release advance feat/alpha` moves it"),
        "include must refuse the second copy and name the way forward: {stdout}"
    );
    assert_eq!(release_parents(&lab, "release/2026-08-04").len(), before);
}

#[test]
fn a_member_that_joined_by_include_is_found_through_the_edit_record() {
    // Given: a cut, then a third branch included, then that branch rebuilt
    // outside jj. The cut event never named it; the include's edit event did.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    assert!(
        included.status.success(),
        "{}",
        String::from_utf8_lossy(&included.stdout)
    );
    let old_gamma = commit_at(&lab, "feat/gamma");
    lab.advance_upstream("upstream advance\n");
    lab.jj_work(["duplicate", "feat/gamma", "-d", "main@upstream"]);
    let new_gamma = CommitId::new(
        lab.revision(lab.work_path(), "children(main@upstream)", "commit_id")
            .trim(),
    );
    lab.jj_work([
        "bookmark",
        "set",
        "feat/gamma",
        "--allow-backwards",
        "-r",
        new_gamma.as_str(),
    ]);
    let before = release_parents(&lab, "release/2026-08-04").len();

    let output = knives_release(&lab, &home, &["advance", "feat/gamma"]);

    assert!(
        output.status.success(),
        "advance refused a member the edit record names: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert!(
        parents.contains(&new_gamma) && !parents.contains(&old_gamma),
        "{parents:?}"
    );
    assert_eq!(parents.len(), before);
}

#[test]
fn a_member_landed_by_merge_commit_has_no_successor_among_fresh_trunk_branches() {
    // Given: alpha landed upstream with a merge commit, so every branch started
    // from the trunk since then descends from alpha's released commit. A fresh
    // gamma is not alpha.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
    let alpha = commit_at(&lab, "feat/alpha");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.merge_pull_with_merge_commit(7);
    lab.jj_work(["new", "main@upstream", "-m", "gamma"]);
    std::fs::write(lab.work_path().join("gamma.txt"), "gamma\n").expect("write gamma");
    lab.jj_work(["bookmark", "create", "feat/gamma", "-r", "@"]);
    lab.jj_work(["new"]);
    let gamma = commit_at(&lab, "feat/gamma");
    let before = release_parents(&lab, "release/2026-08-04");

    // When: gamma is included, and a bare advance runs.
    let included = knives_release(&lab, &home, &["include", "feat/gamma"]);
    let advanced = knives_release(&lab, &home, &["advance"]);

    // Then: gamma joins as a new parent, alpha's landed parent stays untouched
    // (retiring it is `rebase`'s job), and nothing is advanced onto gamma.
    assert!(
        included.status.success(),
        "include misrouted a fresh branch to advance: {}",
        String::from_utf8_lossy(&included.stdout)
    );
    let parents = release_parents(&lab, "release/2026-08-04");
    assert_eq!(parents.len(), before.len() + 1, "{parents:?}");
    assert!(
        parents.contains(&alpha),
        "the landed member was replaced: {parents:?}"
    );
    assert!(parents.contains(&gamma), "gamma did not join: {parents:?}");
    assert!(
        advanced.status.success(),
        "{}",
        String::from_utf8_lossy(&advanced.stdout)
    );
    assert!(
        release_parents(&lab, "release/2026-08-04").contains(&alpha),
        "a bare advance swapped the landed member for a fresh branch"
    );
}

#[test]
fn rebase_names_the_rebased_branch_and_advance_when_a_parent_is_stale() {
    let (lab, home, old_alpha, _new_alpha) = rebased_alpha();

    let output = knives_release(&lab, &home, &["rebase", "main@upstream"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains(&format!("parent {} is stale", &old_alpha.as_str()[..12]))
            && stderr.contains("feat/alpha (now ")
            && stderr.contains("`knives release advance` moves the member"),
        "the refusal must name the branch and the verb that moves it: {stderr}"
    );
    assert!(
        !stderr.contains("Fix the branch"),
        "\"fix the branch\" reads as an instruction to rebuild it on the old base: {stderr}"
    );
}
