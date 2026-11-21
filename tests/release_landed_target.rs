//! What the landed drop of `knives release rebase` keeps so the release does
//! not lose the target.
//!
//! A flat release has no base parent, so the target reaches it only through
//! a member that descends from the target. Where a member that stays does,
//! the drop goes ahead. Where none does, the drop keeps the landed members
//! that do and says why in one line. Where none of those does either, as
//! where only a carrier is on the target, the drop refuses with exit 3 and
//! writes nothing. Where no member would stay, it refuses as before. The
//! same holds whether the rebase moved the release or found that it already
//! contains the target. The shapes are those of INFRA-4895's
//! drop-after-adopt probe (versions 3 and 4), with the forge faked by the
//! snapshot shim. These tests drive only the knives binary.

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
#[path = "common/release_forge.rs"]
mod release_forge;

use std::process::Output;

use forge_shim::pull_record;
use knives::ids::CommitId;
use knives::jj::Repo;
use lab::{
    Lab, commit_at, fast_forward, home_after_first_cut, release_parents, release_ran, squash_land,
    text,
};
use release_forge::knives_release_with_forge;

/// The release every test here cuts and rebases.
const RELEASE: &str = "release/2026-08-04";
/// A member whose pull request #1 upstream merges by a merge commit.
const X: &str = "feat/x";
/// A member whose pull request #2 lands on the target.
const A: &str = "feat/a";
/// A second member squash-landed on the target, as #3.
const B: &str = "feat/b";
/// A member of ours that has not landed.
const C: &str = "feat/c";
/// A member that upstream merges before the target, included after the
/// first cut.
const Y: &str = "feat/y";

/// A lab with each of `members` a branch off the fork's trunk that writes
/// its own file (`feat/x` writes `x.txt`), cut into the release.
fn cut_release(members: &[&str]) -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    for member in members {
        let name = member.trim_start_matches("feat/");
        lab.branch(member, &format!("{name}.txt"), &format!("{name}\n"));
    }
    let (home, _consumer) = home_after_first_cut(&lab);
    (lab, home)
}

/// Upstream merges `branch`'s pull request `number` by a merge commit.
/// Returns the merge commit.
fn merge_land(lab: &Lab, branch: &str, number: u64) -> CommitId {
    lab.publish_pull(branch, number);
    lab.merge_pull_with_merge_commit(number);
    commit_at(lab, "main@upstream")
}

/// Upstream merges X's #1 by a merge commit, then commits `u.txt`. Returns
/// the merge commit.
fn land_x(lab: &Lab) -> CommitId {
    let merged = merge_land(lab, X, 1);
    lab.upstream_trunk_file("u.txt", "upstream\n");
    merged
}

/// The forge's list: each pull request merged as the given commit.
fn merged(pulls: &[(u64, &str, &CommitId)]) -> String {
    let records: Vec<String> = pulls
        .iter()
        .map(|(number, branch, commit)| {
            pull_record(*number, "MERGED", branch, Some(commit.as_str()))
        })
        .collect();
    format!("[{}]", records.join(","))
}

/// One file's text at `revision`, or `None` where it has no such file.
fn file_in(lab: &Lab, revision: &str, file: &str) -> Option<String> {
    knives::jj::file_text(&lab.work, revision, file).expect("read a file")
}

fn sorted(mut commits: Vec<CommitId>) -> Vec<CommitId> {
    commits.sort();
    commits
}

/// The line that says the drop kept each of `branches` on `target`.
fn kept_line(branches: &[&str], target: &CommitId) -> String {
    let target = target.short();
    format!(
        "demo: kept {}: no member but the landed ones is on {target}; dropping them would take \
         {target}'s changes out of {RELEASE}",
        branches.join(", ")
    )
}

/// Assert the release holds `target`'s text of `u.txt` and `a.txt` and
/// descends from it.
fn assert_holds_the_target(lab: &Lab, target: &CommitId, output: &Output) {
    for file in ["u.txt", "a.txt"] {
        let expected = file_in(lab, target.as_str(), file);
        assert!(expected.is_some(), "the target holds no {file}");
        assert_eq!(
            file_in(lab, RELEASE, file),
            expected,
            "{file}: {}",
            text(output)
        );
    }
    let release = commit_at(lab, RELEASE);
    let on_target = Repo::open(&lab.work)
        .expect("open the work checkout")
        .is_ancestor(target, &release)
        .expect("read the ancestry");
    assert!(
        on_target,
        "the release does not descend from the target: {}",
        text(output)
    );
}

#[test]
fn a_bare_rebase_keeps_a_landed_member_that_alone_carries_the_target() {
    // Given (K16 1, the probe's case 2): X and A cut; upstream merges X's #1
    // by a merge commit, commits u.txt, and squash-merges A's #2, which
    // gives T. The forge reports only #2 merged; X's bookmark still holds
    // it.
    let (lab, home) = cut_release(&[X, A]);
    land_x(&lab);
    let target = squash_land(&lab, A, 2);
    let x = commit_at(&lab, X);

    // When: the bare rebase moves onto T and drops what landed.
    let output = knives_release_with_forge(&lab, &home, &merged(&[(2, A, &target)]), &["rebase"]);

    // Then: A stays, and one line says why; the command exits 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&kept_line(&[A], &target)),
        "{}",
        text(&output)
    );
    assert!(!stdout.contains("dropped feat/a"), "{}", text(&output));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    // And: X and A's chain on T stay parents, and the release holds T.
    assert_eq!(
        sorted(release_parents(&lab, RELEASE)),
        sorted(vec![x, commit_at(&lab, A)]),
        "{}",
        text(&output)
    );
    assert_eq!(release_parents(&lab, A), std::slice::from_ref(&target));
    assert_holds_the_target(&lab, &target, &output);
}

#[test]
fn every_member_landed_refuses_and_keeps_the_target() {
    // Given (K16 2): the same shape, with the forge reporting X's #1 merged
    // too.
    let (lab, home) = cut_release(&[X, A]);
    let merged_x = land_x(&lab);
    let target = squash_land(&lab, A, 2);
    let x = commit_at(&lab, X);
    let pulls = merged(&[(1, X, &merged_x), (2, A, &target)]);

    // When: the bare rebase moves onto T and drops what landed.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: the drop refuses, as every member landed, and drops nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        stdout.contains(&format!(
            "demo: every member of {RELEASE} landed; dropping them all would leave it without \
             a parent, so nothing was dropped"
        )),
        "{}",
        text(&output)
    );
    assert_eq!(
        sorted(release_parents(&lab, RELEASE)),
        sorted(vec![x, commit_at(&lab, A)]),
        "{}",
        text(&output)
    );
    // And: the moved release still holds T.
    assert_holds_the_target(&lab, &target, &output);
}

#[test]
fn a_drop_with_another_member_on_the_target_goes_ahead() {
    // Given (K16 3): X, A and C cut, where C's pull request #3 is open; X and
    // A land as in the other tests, and the forge reports #2 merged.
    let (lab, home) = cut_release(&[X, A, C]);
    land_x(&lab);
    let target = squash_land(&lab, A, 2);
    let x = commit_at(&lab, X);
    let pulls = format!(
        "[{},{}]",
        pull_record(2, "MERGED", A, Some(target.as_str())),
        pull_record(3, "OPEN", C, None)
    );

    // When: the bare rebase moves onto T and drops what landed.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: C's rebased tip is on T, so A is dropped, and the command exits
    // 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dropped feat/a: landed upstream as #2"),
        "{}",
        text(&output)
    );
    assert!(!stdout.contains("kept feat/a"), "{}", text(&output));
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    // And: X and C stay, and the release holds T and C's file.
    assert_eq!(
        sorted(release_parents(&lab, RELEASE)),
        sorted(vec![x, commit_at(&lab, C)]),
        "{}",
        text(&output)
    );
    assert_holds_the_target(&lab, &target, &output);
    assert_eq!(file_in(&lab, RELEASE, "c.txt").as_deref(), Some("c\n"));
}

#[test]
fn a_drop_with_no_move_keeps_a_landed_member_that_alone_carries_the_target() {
    // Given (K16 4, the probe's version 4): the shape of the first test, and
    // a rebase onto T with --no-drop, which leaves A's empty chain on T as a
    // parent.
    let (lab, home) = cut_release(&[X, A]);
    land_x(&lab);
    let target = squash_land(&lab, A, 2);
    let x = commit_at(&lab, X);
    let pulls = merged(&[(2, A, &target)]);
    let moved = knives_release_with_forge(
        &lab,
        &home,
        &pulls,
        &["rebase", "--no-drop", target.as_str()],
    );
    assert_eq!(moved.status.code(), Some(0), "{}", text(&moved));
    assert_eq!(
        release_parents(&lab, A),
        std::slice::from_ref(&target),
        "{}",
        text(&moved)
    );
    let before = commit_at(&lab, RELEASE);

    // When: a bare rebase finds that the release already contains T.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: A stays, one line says why, and nothing is written; the command
    // exits 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "demo: {RELEASE} already contains {}",
            target.short()
        )),
        "{}",
        text(&output)
    );
    assert!(
        stdout.contains(&kept_line(&[A], &target)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(commit_at(&lab, RELEASE), before, "{}", text(&output));
    // And: X and A's chain on T stay parents, and the release holds T.
    assert_eq!(
        sorted(release_parents(&lab, RELEASE)),
        sorted(vec![x, commit_at(&lab, A)]),
        "{}",
        text(&output)
    );
    assert_holds_the_target(&lab, &target, &output);
}

#[test]
fn two_landed_members_on_the_target_are_both_kept_and_named() {
    // Given (K16 5): X, A and B cut; X lands as in the other tests, then A's
    // #2 and B's #3 are squash-merged, and B's squash is T. The forge
    // reports #2 and #3 merged.
    let (lab, home) = cut_release(&[X, A, B]);
    land_x(&lab);
    let landed_a = squash_land(&lab, A, 2);
    let target = squash_land(&lab, B, 3);
    let x = commit_at(&lab, X);
    let pulls = merged(&[(2, A, &landed_a), (3, B, &target)]);

    // When: the bare rebase moves onto T and drops what landed.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: both landed members stay, named in one line in the forge's
    // order; the command exits 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&kept_line(&[A, B], &target)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    // And: X, A and B stay parents, and the release holds T.
    assert_eq!(
        sorted(release_parents(&lab, RELEASE)),
        sorted(vec![x, commit_at(&lab, A), commit_at(&lab, B)]),
        "{}",
        text(&output)
    );
    assert_holds_the_target(&lab, &target, &output);
}

#[test]
fn a_release_on_the_target_only_through_a_carrier_refuses_the_drop() {
    // Given (K16 7): X and A cut; upstream merges X's #1 by a merge commit,
    // commits u.txt, and merges A's #2 by a merge commit, which gives T, so
    // both members are in T's history. Then C, made on T, is included, the
    // release pushed, C dropped again, and the write made a fast-forward of
    // the pushed one: the release contains T only through that carrier.
    let (lab, home) = cut_release(&[X, A]);
    land_x(&lab);
    let target = merge_land(&lab, A, 2);
    lab.jj_work(["new", "-r", "main@upstream", "-m", C]);
    std::fs::write(lab.work.join("c.txt"), "c\n").expect("write c.txt");
    lab.jj_work(["bookmark", "create", C, "-r", "@"]);
    lab.jj_work(["new"]);
    release_ran(&lab, &home, &["include", C]);
    let carrier = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    release_ran(&lab, &home, &["drop", C, "--why", "a test"]);
    fast_forward(&lab, RELEASE);
    lab.push_branch(RELEASE);
    assert!(release_parents(&lab, RELEASE).contains(&carrier));
    let before = commit_at(&lab, RELEASE);

    // When: a bare rebase, with the forge reporting only #2, finds that the
    // release already contains T.
    let output = knives_release_with_forge(&lab, &home, &merged(&[(2, A, &target)]), &["rebase"]);

    // Then: the drop refuses, saying no member would carry T, and writes
    // nothing.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    let target = target.short();
    assert!(
        stdout.contains(&format!("demo: {RELEASE} already contains {target}")),
        "{}",
        text(&output)
    );
    assert!(
        stdout.contains(&format!(
            "demo: no member of {RELEASE} would carry {target}: dropping feat/a would take \
             {target}'s changes out of it, so nothing was dropped"
        )),
        "{}",
        text(&output)
    );
    assert_eq!(commit_at(&lab, RELEASE), before, "{}", text(&output));
}

#[test]
fn a_member_on_an_older_trunk_commit_does_not_carry_the_target() {
    // Given (K16 8): A cut alone, then Y included; upstream merges Y's #1 by
    // a merge commit, commits u.txt, and squash-merges A's #2, which gives
    // T, so the trunk holds Y's tip on a commit older than T. A rebase onto
    // T with --no-drop leaves A's empty chain on T and Y where it was.
    let (lab, home) = cut_release(&[A]);
    lab.branch(Y, "y.txt", "y\n");
    release_ran(&lab, &home, &["include", Y]);
    merge_land(&lab, Y, 1);
    lab.upstream_trunk_file("u.txt", "upstream\n");
    let target = squash_land(&lab, A, 2);
    let y = commit_at(&lab, Y);
    let pulls = merged(&[(2, A, &target)]);
    let moved = knives_release_with_forge(
        &lab,
        &home,
        &pulls,
        &["rebase", "--no-drop", target.as_str()],
    );
    assert_eq!(moved.status.code(), Some(0), "{}", text(&moved));
    assert_eq!(commit_at(&lab, Y), y, "{}", text(&moved));
    assert_eq!(
        release_parents(&lab, A),
        std::slice::from_ref(&target),
        "{}",
        text(&moved)
    );

    // When: a bare rebase finds that the release already contains T.
    let output = knives_release_with_forge(&lab, &home, &pulls, &["rebase"]);

    // Then: A stays, and one line says why; the command exits 0.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&kept_line(&[A], &target)),
        "{}",
        text(&output)
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    // And: Y and A's chain on T stay parents, and the release holds T.
    assert_eq!(
        sorted(release_parents(&lab, RELEASE)),
        sorted(vec![y, commit_at(&lab, A)]),
        "{}",
        text(&output)
    );
    assert_holds_the_target(&lab, &target, &output);
}
