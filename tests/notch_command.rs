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
/// Where `a-repo`'s ledger lives: [`UPSTREAM`]'s `<owner>/<name>`, not the
/// registry key, which a store refuses as a name an older knives used.
const LEDGER: &str = "org/work";

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
            "stopped until upstream answers",
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
    lab::state_on_ledger(
        home.path(),
        LEDGER,
        "feat/alpha",
        knives::statement::StatementKind::Pull,
        Some("1157"),
    );

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
    lab::state_on_ledger(
        home.path(),
        LEDGER,
        "feat/alpha",
        knives::statement::StatementKind::Pull,
        Some("1157"),
    );

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
    // A bare read windows to the newest 20 notes and says how many it did not show.
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
    // The statement event is part of feat/alpha's chronology too.
    let parsed: serde_json::Value = serde_json::from_slice(&subject.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 22);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 22);

    // A pull-request read is not windowed either: it is that pull request's
    // whole chronology.
    let pull_request = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "--pr", "1157", "--repo", "a-repo"],
    );
    // The statement event itself is unstamped, so the pull request's chronology
    // is the 21 notes it stamped.
    let parsed: serde_json::Value = serde_json::from_slice(&pull_request.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 21);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 21);
}

#[test]
fn an_unreadable_ledger_is_incomplete_and_an_unknown_repo_is_usage() {
    let checkout = checkout();
    let home = home();

    // Given: a ledger directory holding a file that is not an entry
    let ledger = home.path().join("ledger").join(LEDGER);
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
fn a_verify_past_an_entry_it_cannot_read_is_incomplete_and_names_it() {
    // Given: a note, and an entry of a kind a newer knives writes.
    let checkout = checkout();
    let home = home();
    let wrote = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--text", "notch", "--repo", "a-repo", "-m", "a note"],
    );
    assert!(wrote.status.success(), "{}", stdout(&wrote));
    let unknown = "20990101T000000.000000000Z-0000.md";
    std::fs::write(
        home.path().join("ledger").join(LEDGER).join(unknown),
        "+++\nts = \"2099-01-01T00:00:00Z\"\nowner = \"x\"\nkind = \"verdict\"\n+++\nv\n",
    )
    .expect("an entry of an unknown kind");

    for format in ["--json", "--text"] {
        // When: the ledger's evidence is verified.
        let verified = knives(
            &home,
            &checkout,
            checkout.path(),
            &[format, "notch", "--repo", "a-repo", "--verify"],
        );

        // Then: it did not check everything, and says which file it passed.
        let out = stdout(&verified);
        assert_eq!(verified.status.code(), Some(3), "{format}: {out}");
        assert!(out.contains(unknown), "{format}: {out}");
    }
    let verified = knives(
        &home,
        &checkout,
        checkout.path(),
        &["--json", "notch", "--repo", "a-repo", "--verify"],
    );
    let report: serde_json::Value = serde_json::from_slice(&verified.stdout).expect("JSON");
    assert_eq!(report["checked"], 1, "{report}");
    assert_eq!(
        report["problems"].as_array().map(Vec::len),
        Some(1),
        "{report}"
    );
}

#[test]
fn only_a_note_about_a_branch_reads_the_ledger_for_its_stated_pull_request() {
    // Given: a ledger holding a file that is not an entry
    let checkout = checkout();
    let home = home();
    let ledger = home.path().join("ledger").join(LEDGER);
    std::fs::create_dir_all(&ledger).expect("ledger directory");
    std::fs::write(
        ledger.join("20260815T221403.000000000Z-0000.md"),
        "not a ledger entry at all\n",
    )
    .expect("corrupt entry");
    let write = |args: &[&str]| knives(&home, &checkout, checkout.path(), args);

    // When / Then: a note that asks nothing about a branch's statements is an
    // append, and an append reads no entry
    for args in [
        &[
            "--text",
            "notch",
            "--repo",
            "a-repo",
            "-m",
            "about the fork",
        ][..],
        &[
            "--text",
            "notch",
            "#1157",
            "--repo",
            "a-repo",
            "-m",
            "about a pull request",
        ][..],
        &[
            "--text",
            "notch",
            "feat/alpha",
            "--pr",
            "1157",
            "--repo",
            "a-repo",
            "-m",
            "numbered",
        ][..],
    ] {
        let written = write(args);
        assert_eq!(
            written.status.code(),
            Some(0),
            "{args:?}: {}",
            String::from_utf8_lossy(&written.stderr)
        );
    }

    // And: a note about a branch with no number asks for its stated pull
    // request, so it cannot answer and says which file stopped it
    let branch = write(&[
        "--text",
        "notch",
        "feat/alpha",
        "--repo",
        "a-repo",
        "-m",
        "about a branch",
    ]);
    assert_eq!(branch.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&branch.stderr).contains("0000.md"),
        "the error must name the entry file; was: {}",
        String::from_utf8_lossy(&branch.stderr)
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

#[test]
fn an_empty_message_is_usage_and_does_not_write_a_note_with_no_text() {
    // Given: a repo whose ledger holds nothing yet.
    let checkout = checkout();
    let home = home();
    let note = |message: &str| {
        knives(
            &home,
            &checkout,
            checkout.path(),
            &[
                "--text",
                "notch",
                "feat/log-queue",
                "--evidence",
                "06d778b9",
                "-m",
                message,
                "--repo",
                "a-repo",
            ],
        )
    };
    let matched = || {
        let ledger = knives(
            &home,
            &checkout,
            checkout.path(),
            &["--json", "notch", "--repo", "a-repo"],
        );
        let parsed: serde_json::Value = serde_json::from_slice(&ledger.stdout).expect("JSON");
        parsed["matched"].clone()
    };

    for blank in ["", "   ", "\n"] {
        // When: a note is written with evidence and a message that is empty
        // or only whitespace, as `-m "$(cat file)"` passes after a `cat`
        // that failed.
        let output = note(blank);

        // Then: a usage error that names the flag, and no note is written:
        // its evidence and anchor would say what it is about, and nothing
        // would say what happened.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{blank:?}: {stderr}");
        assert!(
            stderr.contains("--message")
                && stderr.contains("a note must hold text, not be empty or only whitespace"),
            "{blank:?}: {stderr}"
        );
        assert_eq!(matched(), 0, "{blank:?}");
    }

    // When (control): the same command with text.
    let output = note("pushed round 6");

    // Then: the note is written with that text.
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(matched(), 1);
    let shown = stdout(&knives(
        &home,
        &checkout,
        checkout.path(),
        &["--text", "notch", "--repo", "a-repo"],
    ));
    assert!(shown.contains("pushed round 6"), "was: {shown}");
}

/// A registry naming the lab's fork `demo` by its upstream.
fn lab_home(lab: &lab::Lab) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n",
            lab.upstream.display()
        ),
    )
    .expect("write registry");
    home
}

/// `knives --json notch <subject> -m … [--evidence <each>] --repo demo`, run in
/// the lab's work checkout.
fn notch_in(lab: &lab::Lab, home: &tempfile::TempDir, subject: &str, evidence: &[&str]) -> Output {
    let mut args = vec![
        "--json",
        "notch",
        subject,
        "-m",
        "PR head pushed",
        "--repo",
        "demo",
    ];
    for &item in evidence {
        args.extend(["--evidence", item]);
    }
    lab::knives_command(&lab.work, home.path(), lab.temp_path(), &args)
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run knives")
}

/// The anchor of the entry a successful JSON write reports; `None` when the
/// entry has none.
fn written_anchor(output: &Output) -> Option<String> {
    assert_eq!(output.status.code(), Some(0), "{}", lab::text(output));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    parsed["wrote"]
        .get("anchor")
        .map(|anchor| anchor.as_str().expect("an anchor is a string").to_owned())
}

/// `fix-stale` here at one commit and on origin at another that no ancestry
/// joins: what a clone's bookmark keeps when another clone rewrote the branch
/// and pushed it, and this clone fetched without moving its own. Its commit
/// here, then on origin.
fn stale_bookmark(lab: &lab::Lab) -> (String, String) {
    lab.branch("fix-stale", "stale.txt", "the head this clone last made\n");
    lab.foreign_origin_branch(
        "main@origin",
        "fix-stale",
        "the head another clone pushed\n",
    );
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    let here = lab::commit_at(lab, "fix-stale").as_str().to_owned();
    let origin = lab::commit_at(lab, "fix-stale@origin").as_str().to_owned();
    assert_ne!(here, origin, "the fixture's bookmark is not stale");
    (here, origin)
}

/// `fix-gone` only on origin, with no local bookmark: a branch another clone
/// pushed, fetched here and never tracked. Its commit on origin.
fn origin_only_bookmark(lab: &lab::Lab) -> String {
    lab.foreign_origin_branch("main@origin", "fix-gone", "pushed by another clone\n");
    lab.jj_work(["git", "fetch", "--remote", "origin"]);
    lab::commit_at(lab, "fix-gone@origin").as_str().to_owned()
}

#[test]
fn a_note_on_a_stale_bookmark_is_anchored_at_the_origin_head_its_evidence_names() {
    // Given: a local bookmark left behind origin by another clone's push.
    let lab = lab::Lab::new();
    let home = lab_home(&lab);
    let (here, origin) = stale_bookmark(&lab);

    for evidence in [origin.as_str(), &origin[..8]] {
        // When: a note names origin's head as its evidence, whole or as the
        // short id a note's text quotes.
        let wrote = notch_in(&lab, &home, "fix-stale", &[evidence]);

        // Then: the note is anchored at that head, not at the stale local
        // bookmark, and the write warns of nothing.
        assert_eq!(
            written_anchor(&wrote).as_deref(),
            Some(origin.as_str()),
            "{evidence}: anchored behind origin (local is {here}): {}",
            lab::text(&wrote)
        );
        assert!(wrote.stderr.is_empty(), "{evidence}: {}", lab::text(&wrote));
    }

    // When: a note names the local head as its evidence instead.
    let wrote = notch_in(&lab, &home, "fix-stale", &[here.as_str()]);

    // Then: it is anchored there, and nothing is warned: the writer chose that
    // head, so origin's other one is not news.
    assert_eq!(
        written_anchor(&wrote).as_deref(),
        Some(here.as_str()),
        "{}",
        lab::text(&wrote)
    );
    assert!(wrote.stderr.is_empty(), "{}", lab::text(&wrote));
}

#[test]
fn a_note_on_a_branch_only_origin_holds_is_anchored_at_the_head_its_evidence_names() {
    // Given: a branch this clone has only as `fix-gone@origin`.
    let lab = lab::Lab::new();
    let home = lab_home(&lab);
    let origin = origin_only_bookmark(&lab);

    // When: a note names origin's head as its evidence.
    let wrote = notch_in(&lab, &home, "fix-gone", &[origin.as_str()]);

    // Then: the note is anchored at that head rather than left with none.
    assert_eq!(
        written_anchor(&wrote).as_deref(),
        Some(origin.as_str()),
        "{}",
        lab::text(&wrote)
    );
    assert!(wrote.stderr.is_empty(), "{}", lab::text(&wrote));
}

#[test]
fn a_note_without_evidence_keeps_the_local_anchor_and_says_what_origin_holds() {
    // Given: a stale bookmark, a branch only origin holds, and (the control) a
    // branch whose local bookmark is what it pushed to origin.
    let lab = lab::Lab::new();
    let home = lab_home(&lab);
    let (here, origin) = stale_bookmark(&lab);
    let gone = origin_only_bookmark(&lab);
    lab.branch("fix-pushed", "pushed.txt", "pushed\n");
    lab.push_branch("fix-pushed");
    let pushed = lab::commit_at(&lab, "fix-pushed").as_str().to_owned();

    // When: a note on the stale bookmark names no evidence.
    let stale = notch_in(&lab, &home, "fix-stale", &[]);

    // Then: it is anchored at the local bookmark, as before, and the write
    // warns that origin holds another head, and how to anchor there.
    assert_eq!(
        written_anchor(&stale).as_deref(),
        Some(here.as_str()),
        "{}",
        lab::text(&stale)
    );
    let warned = String::from_utf8_lossy(&stale.stderr);
    assert!(
        warned.contains(&format!(
            "warning: fix-stale is at {} here but on origin at {}, so this note is anchored at \
             {}; name the head it is about with --evidence to anchor it there",
            &here[..12],
            &origin[..12],
            &here[..12]
        )),
        "{warned}"
    );

    // When: a note on the branch only origin holds names no evidence.
    let missing = notch_in(&lab, &home, "fix-gone", &[]);

    // Then: it has no anchor, as before, and the write says so.
    assert_eq!(written_anchor(&missing), None, "{}", lab::text(&missing));
    let warned = String::from_utf8_lossy(&missing.stderr);
    assert!(
        warned.contains(&format!(
            "warning: fix-gone resolves to no commit here but is on origin at {}, so this note \
             has no anchor; name the head it is about with --evidence to anchor it there",
            &gone[..12]
        )),
        "{warned}"
    );

    // When (control): a note on the branch whose local bookmark origin holds.
    let agreed = notch_in(&lab, &home, "fix-pushed", &[]);

    // Then: anchored at its tip, with nothing to warn of.
    assert_eq!(
        written_anchor(&agreed).as_deref(),
        Some(pushed.as_str()),
        "{}",
        lab::text(&agreed)
    );
    assert!(agreed.stderr.is_empty(), "{}", lab::text(&agreed));
}

#[test]
fn evidence_that_is_no_head_of_the_subject_does_not_anchor_the_note() {
    // Given: a stale bookmark, and a branch whose local bookmark origin holds.
    let lab = lab::Lab::new();
    let home = lab_home(&lab);
    let (here, origin) = stale_bookmark(&lab);
    lab.branch("fix-pushed", "pushed.txt", "pushed\n");
    lab.push_branch("fix-pushed");
    let pushed = lab::commit_at(&lab, "fix-pushed").as_str().to_owned();
    // The commit a placement note cites: the trunk a reproduction ran on,
    // which every branch here holds and none is at.
    let trunk = lab::commit_at(&lab, "main@origin").as_str().to_owned();

    // When: a note on each cites the trunk commit as its evidence.
    let stale = notch_in(&lab, &home, "fix-stale", &[trunk.as_str()]);
    let agreed = notch_in(&lab, &home, "fix-pushed", &[trunk.as_str()]);

    // Then: neither is anchored at the trunk. The stale one keeps its local
    // anchor and warns that origin holds another head; the other is anchored
    // at its tip with nothing to warn of.
    assert_eq!(
        written_anchor(&stale).as_deref(),
        Some(here.as_str()),
        "{}",
        lab::text(&stale)
    );
    assert!(
        String::from_utf8_lossy(&stale.stderr).contains(&format!(
            "warning: fix-stale is at {} here but on origin at {}",
            &here[..12],
            &origin[..12]
        )),
        "{}",
        lab::text(&stale)
    );
    assert_eq!(
        written_anchor(&agreed).as_deref(),
        Some(pushed.as_str()),
        "{}",
        lab::text(&agreed)
    );
    assert!(agreed.stderr.is_empty(), "{}", lab::text(&agreed));
}
