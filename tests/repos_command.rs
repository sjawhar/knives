//! `knives repos` through the real binary: what the scan found, what it did not,
//! and the one thing it needs from the environment.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::path::Path;

use lab::jj_checkout;
use serde_json::Value;

const TOOL: &str = "https://forge.invalid/org/tool";

/// A config home naming `tool` and `ghost`; neither has consumers, so the
/// listing never asks the forge anything.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.tool]\nupstream = \"{TOOL}\"\norigin = \"https://forge.invalid/acme/tool\"\n\n\
             [repos.ghost]\nupstream = \"https://forge.invalid/org/ghost\"\n\
             origin = \"https://forge.invalid/acme/ghost\"\n"
        ),
    )
    .expect("registry");
    home
}

/// `knives repos` from a directory that is no checkout, scanning `scan_home`.
fn repos(home: &tempfile::TempDir, scan_home: &Path, args: &[&str]) -> std::process::Output {
    let outside = tempfile::tempdir().expect("outside");
    lab::knives_command(outside.path(), home.path(), scan_home, args)
        .output()
        .expect("run knives")
}

#[test]
fn repos_json_lists_every_entry_with_a_null_path_for_one_not_on_this_machine() {
    // A malformed state file is the claims' problem, not the listing's: `repos`
    // takes the sighting but nothing about it can fail the listing.
    let home = home();
    std::fs::write(home.path().join("state.json"), "[[[garbage").expect("garbage state");
    let scan_home = tempfile::tempdir().expect("scan home");
    jj_checkout(&scan_home.path().join("tool"), &[("upstream", TOOL)]);

    let output = repos(&home, scan_home.path(), &["--json", "repos"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");
    let report: Value = serde_json::from_str(&stdout).expect("json document");
    let rows = report["repos"].as_array().expect("rows");
    assert_eq!(rows.len(), 2, "{report}");
    assert_eq!(rows[0]["name"], "ghost");
    assert!(rows[0]["path"].is_null(), "{report}");
    assert_eq!(rows[1]["name"], "tool");
    assert_eq!(
        rows[1]["path"].as_str().map(Path::new),
        Some(
            scan_home
                .path()
                .join("tool")
                .canonicalize()
                .expect("canonical")
                .as_path()
        ),
        "{report}"
    );
    assert!(report.get("problems").is_none(), "{report}");
}

#[test]
fn repos_without_a_home_directory_refuses_rather_than_scanning_the_root() {
    let home = home();
    let outside = tempfile::tempdir().expect("outside");
    let output = lab::knives_command(
        outside.path(),
        home.path(),
        Path::new("/unused"),
        &["--text", "repos"],
    )
    .env_remove("HOME")
    .output()
    .expect("run knives");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert_eq!(
        stderr.trim(),
        "HOME is not set; knives scans $HOME for checkouts"
    );
    assert!(output.stdout.is_empty());
}
