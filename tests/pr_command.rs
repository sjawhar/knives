//! `knives pr` through the real binary.
//!
//! A closed pull and its branch, an unanswered number reported as incomplete,
//! and a timeline whose force pushes carry both tree oids.

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

use forge_shim::{install_snapshot_gh_with_timeline, path_with_gh_shim, pull_record};

use lab::{Lab, release_test_home};
use std::process::Command;

fn knives_pr_with_shim(
    number: u64,
    timeline: bool,
    pulls: &str,
    timeline_nodes: Option<&str>,
) -> std::process::Output {
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh_with_timeline(shim.path(), pulls, timeline_nodes);
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command
        .args(["--text", "pr"])
        .arg(number.to_string())
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()));
    if timeline {
        command.arg("--timeline");
    }
    command.output().expect("run knives pr with a forge shim")
}

#[test]
fn pr_reports_a_closed_pull_and_its_branch_through_the_real_binary() {
    let pulls = format!("[{}]", pull_record(7, "CLOSED", "feat/closed", None));

    let output = knives_pr_with_shim(7, false, &pulls, None);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Ok.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("#7"), "stdout: {stdout}");
    assert!(stdout.contains("CLOSED"), "stdout: {stdout}");
    assert!(stdout.contains("feat/closed"), "stdout: {stdout}");
}

#[test]
fn pr_reports_an_unanswered_number_as_incomplete_through_the_real_binary() {
    let pulls = format!("[{}]", pull_record(7, "CLOSED", "feat/closed", None));

    let output = knives_pr_with_shim(999, false, &pulls, None);

    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("999"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pr_timeline_renders_force_pushes_with_both_tree_oids_through_the_real_binary() {
    let pulls = format!("[{}]", pull_record(7, "CLOSED", "feat/closed", None));
    let timeline = r#"[{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-08-30T22:41:02Z",
        "beforeCommit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "tree":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},
        "afterCommit":{"oid":"cccccccccccccccccccccccccccccccccccccccc",
        "tree":{"oid":"dddddddddddddddddddddddddddddddddddddddd"}}}]"#;

    let output = knives_pr_with_shim(7, true, &pulls, Some(timeline));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("force-push"), "stdout: {stdout}");
    assert!(stdout.contains("tree bbbbbbbbbbbb"), "stdout: {stdout}");
    assert!(stdout.contains("tree dddddddddddd"), "stdout: {stdout}");
}
