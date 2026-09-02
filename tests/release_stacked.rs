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
#[allow(
    dead_code,
    reason = "a shared fixture; not every test file uses every helper"
)]
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
