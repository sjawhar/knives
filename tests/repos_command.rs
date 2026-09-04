//! `knives repos` through the real binary: what the scan found, what it did not,
//! what it could not read, and the one thing it needs from the environment.

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

#[test]
fn repos_names_a_checkout_it_could_not_read_and_is_incomplete() {
    let home = home();
    let scan_home = tempfile::tempdir().expect("scan home");
    jj_checkout(&scan_home.path().join("tool"), &[("upstream", TOOL)]);
    let broken = scan_home.path().join("broken");
    std::fs::create_dir_all(broken.join(".jj").join("repo")).expect("empty store");

    let output = repos(&home, scan_home.path(), &["--text", "repos"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stdout}\n{stderr}");
    let problem = stdout
        .lines()
        .find(|line| line.starts_with("? "))
        .expect("a problem line");
    assert!(problem.contains("reading remotes of"), "{stdout}");
    assert!(problem.contains("broken"), "{stdout}");
    let broken_path = broken.canonicalize().expect("canonical");
    assert_eq!(
        stdout.matches(broken_path.to_str().expect("utf-8")).count(),
        1,
        "said once:\n{stdout}"
    );
    // The found entry is still listed with its path.
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("tool ") && line.contains("/tool")),
        "{stdout}"
    );

    let output = repos(&home, scan_home.path(), &["--json", "repos"]);
    let report: Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json document");
    assert_eq!(output.status.code(), Some(3), "{report}");
    let problems = report["problems"].as_array().expect("problems");
    assert_eq!(problems.len(), 1, "{report}");
}

#[test]
fn two_checkouts_of_one_entry_render_as_ambiguous_with_both_paths_named() {
    let home = home();
    let scan_home = tempfile::tempdir().expect("scan home");
    let one = scan_home.path().join("one");
    let two = scan_home.path().join("two");
    jj_checkout(&one, &[("upstream", TOOL)]);
    jj_checkout(&two, &[("upstream", TOOL)]);

    let output = repos(&home, scan_home.path(), &["--text", "repos"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stdout}\n{stderr}");
    let row = stdout
        .lines()
        .find(|line| line.starts_with("tool "))
        .expect("tool row");
    assert!(row.contains("ambiguous: 2 checkouts"), "{stdout}");
    assert!(!row.contains("not on this machine"), "{stdout}");
    let problem = stdout
        .lines()
        .find(|line| line.trim_start().starts_with("? tool has 2 checkouts"))
        .expect("a problem naming the checkouts");
    let one = one.canonicalize().expect("canonical");
    let two = two.canonicalize().expect("canonical");
    assert!(problem.contains(one.to_str().expect("utf-8")), "{stdout}");
    assert!(problem.contains(two.to_str().expect("utf-8")), "{stdout}");

    let output = repos(&home, scan_home.path(), &["--json", "repos"]);
    let report: Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("json document");
    let tool = report["repos"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["name"] == "tool")
        .expect("tool row");
    assert!(tool["path"].is_null(), "{report}");
    assert!(tool.get("ambiguous").is_none(), "{report}");
}
