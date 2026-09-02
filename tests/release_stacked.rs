//! `knives release` plan: stacked-history detection (field report #1).
//!
//! A member whose own history past the upstream trunk carries a merge — most
//! often a prior release cut — carries everything that merge carried, however
//! flat its own direct parent count looks. These exercise the real defect: a
//! branch built on top of an old release renders "flat" until this fires, and
//! a plain feature branch does not fire at all.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, knives_release, release_test_home};

#[test]
fn a_member_built_on_a_prior_release_is_reported_stacked() {
    // Given: an old dated release built from two branches, then a new branch
    // built directly on top of that old release — carrying its merge into
    // its own history — paired with a plain feature branch and cut as the
    // next release.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    lab.jj_work(["new", "release/2026-08-04", "-m", "feat/gamma content"]);
    lab.jj_work(["bookmark", "create", "feat/gamma", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.branch("feat/delta", "delta.txt", "delta\n");
    lab.octopus("release/2026-08-05", "feat/gamma", "feat/delta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the next release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: the stacked member is named, the release cut it carries is named,
    // and the headline says how many of the parents are stacked.
    assert!(
        text.contains("feat/gamma") && text.contains("carries 1 merge commit(s)"),
        "stacked history not reported: {text}"
    );
    assert!(
        text.contains("release/2026-08-04"),
        "the carried release merge is not named: {text}"
    );
    assert!(
        text.contains("3 parent(s), 1 stacked on a prior merge"),
        "stacked summary not reported: {text}"
    );
}

#[test]
fn linear_members_render_flat_with_no_stacked_history_finding() {
    // Given: a release built from two branches, each forked directly off
    // the trunk with no merge in their own history.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-04", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the release is planned.
    let output = knives_release(&lab, &home, &[]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: no member carries a merge, so the release renders flat.
    assert!(
        text.contains("3 parent(s), flat"),
        "flat summary not reported: {text}"
    );
    assert!(
        !text.contains("stacked on a prior merge"),
        "an unstacked release should not report stacked history: {text}"
    );
}

#[test]
fn a_branch_forked_past_a_stale_upstream_view_is_not_stacked() {
    // Given: a release, then upstream advancing and merging a pull by merge
    // commit, neither fetched from upstream by the work checkout - its
    // `main@upstream` view is behind both - while the fork's trunk mirrors
    // upstream. A new branch forked from the fork's trunk has upstream's own
    // merge in its history past the stale upstream view, both of that merge's
    // parents beyond it, and nothing of its own but linear work.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let (home, _consumer) = release_test_home(&lab);
    let cut = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
    lab.publish_pull("feat/alpha", 7);
    lab.advance_upstream_unfetched("upstream advance\n");
    lab.merge_pull_with_merge_commit_unfetched(7);
    lab.mirror_upstream_trunk_to_origin();
    lab.branch("feat/beta", "beta.txt", "beta\n");

    // When: the plan is read and the branch is included.
    let plan = knives_release(&lab, &home, &[]);
    let plan_text = String::from_utf8_lossy(&plan.stdout).to_string();
    let include = knives_release(&lab, &home, &["include", "feat/beta"]);
    let include_text = String::from_utf8_lossy(&include.stdout).to_string();

    // Then: upstream's own merge is not charged to the branch - the fork's
    // trunk reaches it - so the plan points at include and include takes it.
    assert!(
        !plan_text.contains("feat/beta's history"),
        "a flat branch read as stacked against a stale trunk view: {plan_text}"
    );
    assert!(
        plan_text.contains("feat/beta is not in release/2026-08-04"),
        "{plan_text}"
    );
    assert!(
        include.status.success() && include_text.contains("included feat/beta"),
        "include refused a flat branch: {include_text}\n{}",
        String::from_utf8_lossy(&include.stderr)
    );
}

#[test]
fn the_first_cut_refuses_a_branch_whose_history_carries_a_merge() {
    // Given: no release yet, two plain branches, and a third built by merging
    // both - the shape that would make the first cut carry everything that
    // merge carried while reading as a flat three-parent cut.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("feat/consolidated", "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);

    // When: the first cut is attempted.
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    // Then: it refuses, names the stacked branch, and cuts nothing.
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains("feat/consolidated's history past the trunk carries 1 merge commit(s)")
            && text.contains("rebase it off the trunk before cutting"),
        "the stacked branch must be named and refused: {text}"
    );
    let repo = knives::jj::Repo::open(lab.work_path()).expect("open");
    assert!(
        repo.resolve_commit("release/2026-08-04").is_err(),
        "the cut must not have been published"
    );
}
