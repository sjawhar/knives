#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! The command through the real binary: both output modes, `--repo` from
//! outside the repository, and the exit codes the house rules fix.

#[path = "common/lab.rs"]
mod lab;

use std::path::Path;
use std::process::Output;

const UPSTREAM: &str = "https://forge.invalid/org/work.git";

/// A config home with one managed repo, `a-repo`, known by its upstream.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.a-repo]\nupstream = \"{UPSTREAM}\"\norigin = \"https://forge.invalid/ours/work.git\"\n"
        ),
    )
    .expect("write registry");
    home
}

/// A directory standing in for `$HOME`, holding the one jj checkout whose
/// `upstream` remote is `a-repo`'s, so `--repo a-repo` finds it from anywhere.
fn checkout() -> tempfile::TempDir {
    let scan_root = tempfile::tempdir().expect("checkout");
    lab::jj_checkout(&scan_root.path().join("a-repo"), &[("upstream", UPSTREAM)]);
    scan_root
}

fn knives(
    home: &tempfile::TempDir,
    scan_root: &tempfile::TempDir,
    cwd: &Path,
    args: &[&str],
) -> Output {
    lab::knives_command(cwd, home.path(), scan_root.path(), args)
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run knives")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_note_written_from_outside_the_repo_is_read_back_in_both_modes() {
    // Given: a config home naming one repo, and a cwd that is not it — the case
    // the --repo flag exists for: you learn something about the library fork
    // while standing in the consumer fork.
    let checkout = checkout();
    let home = home();
    let elsewhere = tempfile::tempdir().expect("somewhere else");

    // When: a note is written for that repo by name
    let wrote = knives(
        &home,
        &checkout,
        elsewhere.path(),
        &[
            "--text",
            "notch",
            "feat/log-queue",
            "-m",
            "superseded by #1157; upstream wanted the trait approach",
            "--evidence",
            "06d778b9",
            "--repo",
            "a-repo",
        ],
    );

    // Then: it succeeded and said what it recorded
    assert_eq!(
        wrote.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    assert_eq!(
        stdout(&wrote).trim(),
        "notched feat/log-queue",
        "was: {}",
        stdout(&wrote)
    );

    // And: the prose read shows the entry, its kind and its evidence
    let text = knives(
        &home,
        &checkout,
        elsewhere.path(),
        &["--text", "notch", "--repo", "a-repo"],
    );
    let shown = stdout(&text);
    assert!(shown.contains("note"), "was: {shown}");
    assert!(shown.contains("superseded by #1157"), "was: {shown}");
    assert!(shown.contains("06d778b9"), "was: {shown}");

    // And: the JSON read carries the same facts as fields
    let json = knives(
        &home,
        &checkout,
        elsewhere.path(),
        &["--json", "notch", "--repo", "a-repo"],
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("notch --json emits JSON");
    assert_eq!(parsed["repo"], "a-repo");
    assert_eq!(parsed["matched"], 1);
    assert_eq!(parsed["entries"][0]["kind"], "note");
    assert_eq!(parsed["entries"][0]["owner"], "ses_fff688");
    assert_eq!(parsed["entries"][0]["subject"], "feat/log-queue");
    assert_eq!(parsed["entries"][0]["evidence"][0], "06d778b9");
    // The checkout has no bookmark of that name, so the subject's tip does not
    // resolve and the entry says so by omission.
    assert!(parsed["entries"][0].get("anchor").is_none());
}

#[test]
fn the_machine_default_is_toon_and_decodes_to_exactly_the_json_report() {
    // Given: a written note. The machine default changed from JSON to TOON for
    // token cost; the contract is that nothing else changed — the TOON output
    // is the same report, losslessly.
    let checkout = checkout();
    let home = home();
    let wrote = knives(
        &home,
        &checkout,
        checkout.path(),
        &[
            "--text",
            "notch",
            "feat/alpha",
            "-m",
            "parked until upstream answers",
            "--evidence",
            "06d778b9",
            "--repo",
            "a-repo",
        ],
    );
    assert_eq!(wrote.status.code(), Some(0), "{wrote:?}");

    // When: the report is read with no format flag (stdout is a pipe, so the
    // machine default applies) and once more with explicit --json.
    let bare = knives(
        &home,
        &checkout,
        checkout.path(),
        &["notch", "--repo", "a-repo"],
    );
    let json = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "--repo", "a-repo"],
    );

    // Then: the bare output is TOON, not JSON, and decodes to the identical value.
    let toon_text = stdout(&bare);
    assert!(
        !toon_text.trim_start().starts_with('{'),
        "the machine default still emits JSON: {toon_text}"
    );
    let from_toon: serde_json::Value =
        toon_format::decode_default(&toon_text).expect("machine default decodes as TOON");
    let from_json: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("--json emits JSON");
    assert_eq!(from_toon, from_json, "TOON and JSON reports diverged");
}

#[test]
fn a_json_write_emits_only_the_entry_it_wrote() {
    let checkout = checkout();
    let home = home();
    let elsewhere = tempfile::tempdir().expect("somewhere else");

    let wrote = knives(
        &home,
        &checkout,
        elsewhere.path(),
        &[
            "--json",
            "notch",
            "feat/alpha",
            "-m",
            "recorded a decision",
            "--repo",
            "a-repo",
        ],
    );

    assert_eq!(wrote.status.code(), Some(0));
    let parsed: serde_json::Value = serde_json::from_slice(&wrote.stdout).expect("JSON");
    assert_eq!(
        parsed.as_object().expect("object").len(),
        1,
        "was: {parsed}"
    );
    assert_eq!(parsed["wrote"]["text"], "recorded a decision");
    assert!(parsed.get("repo").is_none(), "was: {parsed}");
    assert!(parsed.get("entries").is_none(), "was: {parsed}");
    assert!(parsed.get("matched").is_none(), "was: {parsed}");
}

#[test]
fn a_write_pr_stamps_the_entry_and_a_pr_read_finds_it() {
    let checkout = checkout();
    let home = home();
    let elsewhere = tempfile::tempdir().expect("somewhere else");

    let wrote = knives(
        &home,
        &checkout,
        elsewhere.path(),
        &[
            "--json",
            "notch",
            "feat/alpha",
            "-m",
            "stated for this pull request",
            "--pr",
            "4891",
            "--repo",
            "a-repo",
        ],
    );
    assert_eq!(wrote.status.code(), Some(0));
    let written: serde_json::Value = serde_json::from_slice(&wrote.stdout).expect("JSON");
    assert_eq!(written["wrote"]["pr"], 4891);

    let read = knives(
        &home,
        &checkout,
        elsewhere.path(),
        &["--json", "notch", "--pr", "4891", "--repo", "a-repo"],
    );
    assert_eq!(read.status.code(), Some(0));
    let entries: serde_json::Value = serde_json::from_slice(&read.stdout).expect("JSON");
    assert_eq!(entries["matched"], 1);
    assert_eq!(entries["entries"][0]["pr"], 4891);
}

#[test]
fn a_write_without_pr_uses_the_tracked_pull_stamp() {
    let checkout = checkout();
    let home = home();
    std::fs::write(
        home.path().join("state.json"),
        r#"{"tracked_pulls":{"a-repo/feat/alpha":1157}}"#,
    )
    .expect("seed tracked pull");

    let wrote = knives(
        &home,
        &checkout,
        checkout.path(),
        &[
            "--json",
            "notch",
            "feat/alpha",
            "-m",
            "uses tracked pull",
            "--repo",
            "a-repo",
        ],
    );
    assert_eq!(wrote.status.code(), Some(0));
    let written: serde_json::Value = serde_json::from_slice(&wrote.stdout).expect("JSON");
    assert_eq!(written["wrote"]["pr"], 1157);
}

#[test]
fn a_subject_read_shows_that_refs_chronology_and_a_bare_read_windows_the_repo() {
    let checkout = checkout();
    let home = home();
    std::fs::write(
        home.path().join("state.json"),
        r#"{"tracked_pulls":{"a-repo/feat/alpha":1157}}"#,
    )
    .expect("seed tracked pull");

    for index in 0..42 {
        let text = format!("entry {index}");
        let subject = if index % 2 == 0 {
            "feat/alpha"
        } else {
            "feat/beta"
        };
        let wrote = knives(
            &home,
            &checkout,
            checkout.path(),
            &["--text", "notch", subject, "-m", &text, "--repo", "a-repo"],
        );
        assert_eq!(wrote.status.code(), Some(0));
    }

    // A bare read windows to the newest 20 and says how many it did not show.
    let bare = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "--repo", "a-repo"],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&bare.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 42);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 20);
    assert_eq!(parsed["entries"][0]["text"], "entry 22");

    // A subject read is not windowed: it is that ref's whole chronology.
    let subject = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "feat/alpha", "--repo", "a-repo"],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&subject.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 21);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 21);

    // A pull-request read is not windowed either: it is that pull request's
    // whole chronology.
    let pull_request = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "--pr", "1157", "--repo", "a-repo"],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&pull_request.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 21);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 21);
}

#[test]
fn an_unreadable_ledger_is_incomplete_and_an_unknown_repo_is_usage() {
    let checkout = checkout();
    let home = home();

    // Given: a ledger directory holding a file that is not an entry
    let ledger = home.path().join("ledger").join("a-repo");
    std::fs::create_dir_all(&ledger).expect("ledger directory");
    std::fs::write(
        ledger.join("20260815T221403.000000000Z-0000.md"),
        "not a ledger entry at all\n",
    )
    .expect("corrupt entry");

    // When / Then: reading it cannot answer, and says so with exit 3
    let broken = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--text", "notch", "--repo", "a-repo"],
    );
    assert_eq!(broken.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&broken.stderr).contains("0000.md"),
        "the error must name the entry file; was: {}",
        String::from_utf8_lossy(&broken.stderr)
    );

    // And: a repo nobody manages is a usage error, naming the ones we do
    let unknown = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--text", "notch", "--repo", "nope"],
    );
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("a-repo"),
        "was: {}",
        String::from_utf8_lossy(&unknown.stderr)
    );
}

#[test]
fn a_read_of_a_repo_with_no_ledger_yet_is_success_and_says_so() {
    let checkout = checkout();
    let home = home();
    let empty = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--text", "notch", "--repo", "a-repo"],
    );
    assert_eq!(empty.status.code(), Some(0));
    assert!(
        stdout(&empty).contains("no notches"),
        "was: {}",
        stdout(&empty)
    );
}

#[test]
fn a_read_never_asks_who_is_reading_and_a_write_is_stopped_by_an_unreadable_state() {
    // Resolving who is acting reads state.json (a terminal user inside a fork
    // is named by the fork's claims). A read writes nothing in anyone's name,
    // so an unreadable state file is not its problem; a write is stopped by it.
    let checkout = checkout();
    let home = home();
    std::fs::write(home.path().join("state.json"), "{not json").expect("corrupt state");
    let anonymous = |args: &[&str]| {
        lab::knives_command(
            &checkout.path().join("a-repo"),
            home.path(),
            checkout.path(),
            args,
        )
        .env_remove("KNIVES_OWNER")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .output()
        .expect("run knives")
    };

    for read in [
        &["--text", "notch"][..],
        &["--text", "notch", "--verify"],
        &["--text", "notch", "--events"],
    ] {
        let output = anonymous(read);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{read:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("state.json"),
            "{read:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let write = anonymous(&["--text", "notch", "feat/alpha", "-m", "a note"]);
    assert_eq!(write.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&write.stderr).contains("state.json"),
        "was: {}",
        String::from_utf8_lossy(&write.stderr)
    );
}

#[test]
fn an_empty_subject_is_usage_and_does_not_write_a_nameless_entry() {
    let checkout = checkout();
    let home = home();

    let empty_read = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--text", "notch", "", "--repo", "a-repo"],
    );
    assert_eq!(empty_read.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&empty_read.stderr).contains("subject"),
        "was: {}",
        String::from_utf8_lossy(&empty_read.stderr)
    );

    let empty_write = knives(
        &home,
        &checkout,
        checkout.path(),
        &[
            "--text",
            "notch",
            "",
            "-m",
            "must name a branch",
            "--repo",
            "a-repo",
        ],
    );
    assert_eq!(empty_write.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&empty_write.stderr).contains("subject"),
        "was: {}",
        String::from_utf8_lossy(&empty_write.stderr)
    );

    let whitespace_write = knives(
        &home,
        &checkout,
        checkout.path(),
        &[
            "--text",
            "notch",
            " ",
            "-m",
            "must name a branch",
            "--repo",
            "a-repo",
        ],
    );
    assert_eq!(whitespace_write.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&whitespace_write.stderr).contains("subject"),
        "was: {}",
        String::from_utf8_lossy(&whitespace_write.stderr)
    );

    let ledger = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "--repo", "a-repo"],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&ledger.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 0);
}
