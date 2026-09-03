//! Re-adopting a registered checkout keeps what the registry was told by hand.
//!
//! `init` and `register` read remotes. `base`, `release_branch`, `test_count_command`,
//! `consumers` and `workspaces` are written by hand and nothing on disk can recover
//! them; rebuilding the entry from remotes alone silently moved every new workspace
//! back beside the checkout.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::Lab;
use std::process::Command;

const HAND_WRITTEN: &str = "base = \"main\"\nrelease_branch = \"sami\"\n\
                            test_count_command = \"printf 10\"\nconsumers = [\"acme/workbench\"]\n";

/// A registry whose entry for the lab checkout carries every hand-written field.
fn registered_home(lab: &Lab) -> (tempfile::TempDir, String) {
    let home = tempfile::tempdir().expect("create config home");
    let workspaces = home.path().join("worktrees").join("work");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.work]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"{}\"\n{HAND_WRITTEN}\
             workspaces = \"{}\"\n",
            lab.work.display(),
            lab.upstream.display(),
            lab.work.display(),
            workspaces.display(),
        ),
    )
    .expect("write registry");
    (home, workspaces.display().to_string())
}

fn knives(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(args)
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .output()
        .expect("run knives")
}

fn assert_carries_hand_written_fields(text: &str, workspaces: &str) {
    for line in HAND_WRITTEN.lines() {
        assert!(text.contains(line), "lost `{line}`:\n{text}");
    }
    assert!(
        text.contains(&format!("workspaces = \"{workspaces}\"")),
        "lost `workspaces`:\n{text}"
    );
}

#[test]
fn init_on_a_registered_checkout_keeps_its_hand_written_fields() {
    // Given: a registered checkout with every optional field set
    let lab = Lab::new();
    let (home, workspaces) = registered_home(&lab);

    // When: init runs on it again
    let output = knives(&lab, &home, &["--text", "init"]);

    // Then: the rewritten registry still carries every hand-written field
    assert!(
        output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rewritten =
        std::fs::read_to_string(home.path().join("repos.toml")).expect("read rewritten registry");
    assert_carries_hand_written_fields(&rewritten, &workspaces);
}

#[test]
fn register_on_a_registered_checkout_prints_a_snippet_with_its_hand_written_fields() {
    // Given: a registered checkout with every optional field set. The snippet is
    // what a human pastes over the existing entry, so a snippet without these
    // fields is an instruction to lose them.
    let lab = Lab::new();
    let (home, workspaces) = registered_home(&lab);

    // When: register prints its snippet
    let output = knives(&lab, &home, &["--text", "register"]);

    // Then: the snippet carries every hand-written field
    assert!(
        output.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_carries_hand_written_fields(&String::from_utf8_lossy(&output.stdout), &workspaces);
}
