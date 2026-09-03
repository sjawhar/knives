//! `probe_landed`: whether a branch's content reached the trunk, ancestry aside.
//!
//! A squash merge lands content ancestry cannot see. The probe replays the
//! branch onto the trunk in scratch commits it always cleans up, writes nothing
//! to the shared op log, never abandons a commit it did not create, forks and
//! probes against the configured trunk, and answers the same from many threads
//! as from one.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::detect::landed::RebaseOutcome;
use knives::ids::{BookmarkRef, BranchName};
use knives::jj::{Repo, probe_landed};
use lab::{Lab, operation_ids};
use std::process::Command;

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
fn jj_lib_answers_the_same_probe_from_many_threads_as_from_one() {
    // Every parallel landed probe opens its own repository handle and replays
    // inside a transaction it drops. jj's own model is concurrent-safe by design,
    // but the loaded-repo handle is not assumed Sync, so this is measured rather
    // than believed. The operation log must also remain unchanged after all
    // concurrent probes complete.
    let lab = lab::Lab::new();
    for index in 0..8 {
        lab.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    let branches: Vec<BranchName> = (0..8)
        .map(|index| BranchName::new(format!("feat/b{index}")))
        .collect();

    // When: the same probes run serially and then all at once
    let serial: Vec<RebaseOutcome> = branches
        .iter()
        .map(|branch| probe_landed(&lab.work, branch, "main@upstream").expect("serial probe"))
        .collect();
    let work = lab.work.as_path();
    let operations_before = operation_ids(&lab.work);
    let concurrent: Vec<RebaseOutcome> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(branches.len());
        for branch in &branches {
            handles.push(scope.spawn(move || {
                probe_landed(work, branch, "main@upstream").expect("concurrent probe")
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a probe thread panicked"))
            .collect()
    });

    // Then: every answer is identical and in the same order
    assert_eq!(
        concurrent, serial,
        "a concurrent probe answered differently from a serial one"
    );
    assert!(
        serial
            .iter()
            .all(|outcome| *outcome == RebaseOutcome::CleanNonEmpty),
        "the fixture's unmerged branches should all be unlanded: {serial:?}"
    );
    // And: concurrent probes wrote no operation into the shared log.
    assert_eq!(operation_ids(&lab.work), operations_before);

    // And: the repository is still readable afterwards and retains every branch.
    let tips = Repo::open(&lab.work)
        .expect("reopen after concurrent probes")
        .bookmark_tips()
        .expect("read tips");
    assert!(
        (0..8).all(
            |index| tips.contains_key(&BookmarkRef::Local(BranchName::new(format!(
                "feat/b{index}"
            ))))
        ),
        "a feature bookmark disappeared after concurrent probes: {tips:?}"
    );
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
