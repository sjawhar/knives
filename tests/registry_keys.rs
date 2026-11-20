//! The keys of a registry entry and of its series tables, through the binary.
//!
//! A shared registry can carry a key that only a newer knives reads. An entry
//! or series table names such a key on standard error and ignores it, so a
//! machine that runs an older knives still reads the entry, and standard
//! output stays the command's one document.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, knives_release, release_test_home};
use serde_json::Value;

#[test]
fn an_unknown_key_in_an_entry_warns_and_is_ignored() {
    // Given: an entry that carries a key no knives reads, and a series table,
    // whose name `9.2` is no bare TOML key, that carries another.
    let home = tempfile::tempdir().expect("config home");
    let registry = home.path().join("repos.toml");
    std::fs::write(
        &registry,
        "[repos.tool]\nupstream = \"https://forge.invalid/org/tool\"\n\
         origin = \"https://forge.invalid/acme/tool\"\nfuture_key = 1\n\n\
         [repos.tool.series.\"9.2\"]\nbase = \"old\"\nstacked2 = true\n",
    )
    .expect("write registry");
    let scan_home = tempfile::tempdir().expect("scan home");
    let outside = tempfile::tempdir().expect("outside");

    // When: the listing runs.
    let output = lab::knives_command(
        outside.path(),
        home.path(),
        scan_home.path(),
        &["--json", "repos"],
    )
    .output()
    .expect("run knives");

    // Then: it exits 0 with one JSON document on standard output that lists
    // the entry, and standard error names each key and its table once, with
    // the series name quoted as the file writes it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stdout}\n{stderr}");
    let report: Value = serde_json::from_str(&stdout).expect("json document");
    assert_eq!(report["repos"][0]["name"], "tool", "{report}");
    for line in [
        format!(
            "knives: warning: {}: [repos.tool] has an unknown key future_key; it is ignored\n",
            registry.display()
        ),
        format!(
            "knives: warning: {}: [repos.tool.series.\"9.2\"] has an unknown key stacked2; it \
             is ignored\n",
            registry.display()
        ),
    ] {
        assert_eq!(stderr.matches(&line).count(), 1, "{line}: {stderr}");
    }
}

#[test]
fn a_series_reads_its_stacked_key() {
    // Given: an upstream with a second base, `old`, and a registry whose series
    // on it sets `stacked = true`.
    let lab = Lab::new();
    lab.upstream_branch("old", "old.txt", "old base\n");
    lab.advance_upstream("new base\n");
    let (home, _consumer) = release_test_home(&lab);
    let registry = home.path().join("repos.toml");
    let mut text = std::fs::read_to_string(&registry).expect("read registry");
    text.push_str("\n[repos.demo.series.old]\nbase = \"old\"\nstacked = true\n");
    std::fs::write(&registry, text).expect("write registry");

    // When: the series' release plan runs.
    let plan = knives_release(&lab, &home, &["--series", "old"]);

    // Then: the registry loads with no warning, and the plan answers for the
    // series.
    let stdout = String::from_utf8_lossy(&plan.stdout);
    let stderr = String::from_utf8_lossy(&plan.stderr);
    assert_eq!(plan.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(!stderr.contains("knives: warning"), "{stderr}");
    assert!(stdout.contains("series old has no cut yet"), "{stdout}");
}
