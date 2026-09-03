//! `knives preflight`: what a branch would face, read before starting.
//!
//! Names the configured trunk even when it is `dev`, treats a fixed release
//! branch as a release rather than a branch, hides a divergent configured trunk
//! bookmark and flags a branch whose tip is divergent.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::config::RepoEntry;

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
        workspaces: None,
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
        workspaces: None,
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
        workspaces: None,
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
        workspaces: None,
    };
    let states = knives::commands::preflight::branch_states(&entry, &[]).expect("branch states");
    assert!(
        states.iter().any(|state| state.divergent),
        "a branch whose tip is divergent must be reported as divergent, got {states:#?}"
    );
}
