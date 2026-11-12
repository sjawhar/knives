//! The claim on a branch, from `start` to `finish`.
//!
//! One harness session resumes its own claim; another is refused and told who
//! holds it; `--force` seizes and records the previous owner; possession of the
//! workspace resumes by itself. Workspaces are adopted, refused or reported
//! missing rather than rebuilt. Provenance is written atomically or not at all,
//! and every run leaves its owner and workspace in the last-seen record before
//! the command does anything else.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::jj::Repo;
use lab::{Lab, operation_ids, release_test_home};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

/// Make the ledger directory `ledger` readable and unwritable, so a store
/// open reads it and only an append fails; `false` when this user writes it
/// anyway (root ignores modes), and the caller has nothing to test.
fn unwritable(ledger: &Path) -> bool {
    std::fs::set_permissions(ledger, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    if tempfile::NamedTempFile::new_in(ledger).is_ok() {
        std::fs::set_permissions(ledger, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return false;
    }
    true
}

/// The entry of `entries` whose text starts with `prefix`.
fn entry_starting<'a>(entries: &'a [Value], prefix: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| {
            entry["text"]
                .as_str()
                .is_some_and(|text| text.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("no entry whose text starts with {prefix:?}: {entries:?}"))
}

/// Whether `token` is a durable last-seen token: a timestamp, or one of the
/// two words for a window that saw nothing. The age a refusal prints ("3h
/// ago") would still read as three hours next year.
fn durable_sighting(token: &Value) -> bool {
    token.as_str().is_some_and(|token| {
        token == "none-since-claim"
            || token == "none-within-window"
            || token.parse::<jiff::Timestamp>().is_ok()
    })
}

/// Run knives in `lab`'s checkout as the harness session `owner`, against
/// the config home `home`, and fail the test when it does not exit 0.
fn ran(lab: &Lab, home: &Path, owner: &str, args: &[&str]) -> std::process::Output {
    let output = tried(lab, home, owner, args);
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// [`ran`], for a run whose exit the test reads itself: a refusal and a
/// finding are what these tests are about, not failures of the fixture.
fn tried(lab: &Lab, home: &Path, owner: &str, args: &[&str]) -> std::process::Output {
    lab::knives_command(&lab.work, home, lab.temp_path(), args)
        .env("KNIVES_OWNER", owner)
        .output()
        .expect("run knives")
}

/// Every entry of the demo fork's ledger in `home`, oldest first.
fn ledger_of(home: &Path) -> Vec<knives::ledger::Entry> {
    knives::ledger::Ledger::at(home.join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries
}

/// The entry of the demo fork's ledger that records claim change `change`,
/// newest first.
fn claim_change(home: &Path, change: &str) -> knives::ledger::Entry {
    ledger_of(home)
        .into_iter()
        .rev()
        .find(|entry| entry.claim_change.as_deref() == Some(change))
        .unwrap_or_else(|| panic!("no {change} entry in {:?}", ledger_of(home)))
}

/// Who holds `demo/<branch>` in `home`'s state, when anybody does.
fn holder_of(home: &Path, branch: &str) -> Option<String> {
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("state.json")).ok()?)
            .expect("parse state");
    state["claims"][format!("demo/{branch}")]["owner"]
        .as_str()
        .map(ToOwned::to_owned)
}

/// Rewrite the demo fork's ledger as an older knives wrote it: every entry
/// without the claim fields, which that knives had no word for.
fn without_claim_fields(home: &Path) {
    let ledger = home.join("ledger").join("demo");
    for dirent in std::fs::read_dir(&ledger).expect("read ledger directory") {
        let path = dirent.expect("ledger entry").path();
        let text = std::fs::read_to_string(&path).expect("read entry");
        let older: String = text
            .lines()
            .filter(|line| !line.starts_with("claim_change") && !line.starts_with("previous_"))
            .fold(String::new(), |mut text, line| {
                text.push_str(line);
                text.push('\n');
                text
            });
        std::fs::write(&path, older).expect("rewrite entry");
    }
}

/// Make the ledger directory `ledger` unreadable, so the store fails on it
/// before anything else; `false` when this user reads it anyway (root
/// ignores modes), and the caller has nothing to test.
fn unreadable(ledger: &Path) -> bool {
    std::fs::set_permissions(ledger, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::fs::read_dir(ledger).is_ok() {
        std::fs::set_permissions(ledger, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return false;
    }
    true
}

#[test]
fn start_resumes_the_same_harness_sessions_claim_without_mutating_it() {
    // A second invocation from the same harness session must acknowledge the
    // existing claim rather than overwrite its timestamp or reason.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_OWNER", "agent-one")
            .output()
            .expect("run start")
    };

    let first = run(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--why",
        "port it",
    ]);
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let state_before = std::fs::read_to_string(home.path().join("state.json")).expect("state");

    let second = run(&["--text", "start", "feat/gamma", "--repo", "demo"]);

    assert!(
        second.status.success(),
        "resume must exit 0: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("resumed"), "stdout: {stdout}");
    assert!(stdout.contains("feat-gamma"), "stdout: {stdout}");
    assert_eq!(
        std::fs::read_to_string(home.path().join("state.json")).expect("state"),
        state_before,
        "resume must not rewrite the claim"
    );

    let events = run(&[
        "--text",
        "notch",
        "feat/gamma",
        "--events",
        "--repo",
        "demo",
    ]);
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    assert!(
        String::from_utf8_lossy(&events.stdout).contains("resumed"),
        "events: {}",
        String::from_utf8_lossy(&events.stdout)
    );
}

/// The whole claim on `demo/<branch>` in `home`'s state, or `Value::Null`
/// where none is held: a resume that writes the reason must be read for
/// what it left alone as much as for what it wrote.
fn claim_of(home: &Path, branch: &str) -> Value {
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("state.json")).expect("state"))
            .expect("parse state");
    state["claims"][format!("demo/{branch}")].clone()
}

/// [`tried`]'s command, built to run in `cwd` instead of the checkout, with
/// no harness variable set: standing in the claimed workspace is what the
/// caller is proving, and the identity is the caller's own to state.
fn knives_at(lab: &Lab, home: &Path, cwd: &Path, args: &[&str]) -> Command {
    lab::knives_command(cwd, home, lab.temp_path(), args)
}

#[test]
fn start_with_a_reason_restates_the_holders_claim_without_moving_when_it_was_taken() {
    // Measured with v3.2.4: a claim three days old still gave the reason its
    // first `start` gave, so every refusal naming it was out of date. The
    // holder saying why again replaces it; when and where the claim was
    // taken are facts of that moment and stay as they are.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = claimed_by_agent_one(&lab, home.path());
    let taken = claim_of(home.path(), "feat/gamma");
    assert!(
        taken["place"].is_string(),
        "the fixture must record where the claim was taken: {taken}"
    );

    let resumed = ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "finish the port",
        ],
    );

    let stdout = String::from_utf8_lossy(&resumed.stdout);
    assert!(stdout.contains("resumed"), "stdout: {stdout}");
    assert!(
        stdout.contains("finish the port"),
        "the resume must show the reason the claim now holds: {stdout}"
    );
    assert!(
        !stdout.contains("port it"),
        "the replaced reason must not be what the reader is told: {stdout}"
    );
    let now = claim_of(home.path(), "feat/gamma");
    assert_eq!(now["why"], "finish the port");
    assert_eq!(now["started"], taken["started"], "claim now: {now}");
    assert_eq!(now["place"], taken["place"], "claim now: {now}");
    assert_eq!(now["owner"], taken["owner"], "claim now: {now}");
    assert_eq!(now["kind"], taken["kind"], "claim now: {now}");
    let entry = claim_change(home.path(), "resumed");
    assert_eq!(entry.text, "resumed");
    assert_eq!(
        entry.claim_change_why.as_deref(),
        Some("finish the port"),
        "the entry carries the reason the claim holds after it: {entry:?}"
    );
}

#[test]
fn start_with_a_reason_from_inside_the_workspace_restates_the_holders_claim() {
    // The holder reaches its own claim through either arm of a resume: its
    // name from outside the directory, possession from inside it. The same
    // `--why` must land the same way.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = claimed_by_agent_one(&lab, home.path());
    let taken = claim_of(home.path(), "feat/gamma");

    let resumed = knives_at(
        &lab,
        home.path(),
        &workspace,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "finish the port",
        ],
    )
    .env("KNIVES_OWNER", "agent-one")
    .output()
    .expect("resume from the workspace");

    let stdout = String::from_utf8_lossy(&resumed.stdout);
    assert!(
        resumed.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(
        stdout.contains("finish the port"),
        "the resume must show the reason the claim now holds: {stdout}"
    );
    let now = claim_of(home.path(), "feat/gamma");
    assert_eq!(now["why"], "finish the port");
    assert_eq!(now["started"], taken["started"], "claim now: {now}");
    assert_eq!(now["place"], taken["place"], "claim now: {now}");
    let entry = claim_change(home.path(), "resumed");
    assert_eq!(entry.text, "resumed via workspace possession");
    assert_eq!(
        entry.claim_change_why.as_deref(),
        Some("finish the port"),
        "the entry carries the reason the claim holds after it: {entry:?}"
    );
}

#[test]
fn start_without_a_reason_leaves_the_one_the_claim_holds() {
    // A resume that says nothing says nothing: the reason is the holder's
    // last word on the work, not a field every `start` blanks.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = claimed_by_agent_one(&lab, home.path());
    let taken = claim_of(home.path(), "feat/gamma");

    let resumed = ran(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    assert!(
        String::from_utf8_lossy(&resumed.stdout).contains("port it"),
        "stdout: {}",
        String::from_utf8_lossy(&resumed.stdout)
    );
    assert_eq!(claim_of(home.path(), "feat/gamma"), taken);
    let entry = claim_change(home.path(), "resumed");
    assert_eq!(entry.claim_change_why.as_deref(), Some("port it"));
}

#[test]
fn start_with_a_reason_from_another_name_in_the_workspace_leaves_the_claim_alone() {
    // Possession resumes the work, not the claim: a session of another name
    // standing in the directory is working there, and the reason the branch
    // is held is still its holder's to give.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = claimed_by_agent_one(&lab, home.path());
    let taken = claim_of(home.path(), "feat/gamma");

    let resumed = knives_at(
        &lab,
        home.path(),
        &workspace,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "pick up the rebase",
        ],
    )
    .env("KNIVES_OWNER", "agent-two")
    .output()
    .expect("resume from the workspace");

    assert!(
        resumed.status.success(),
        "possession resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        claim_of(home.path(), "feat/gamma"),
        taken,
        "another name's resume writes nothing to the claim"
    );
    let entry = claim_change(home.path(), "resumed");
    assert_eq!(entry.text, "resumed via workspace possession");
    assert_eq!(
        entry.claim_change_why.as_deref(),
        Some("port it"),
        "the entry carries the reason the claim holds: {entry:?}"
    );
}

#[test]
fn start_with_a_reason_from_the_derived_holder_restates_the_claim() {
    // With no harness naming the session, the sole claimant of the fork is
    // who the caller is taken to be. That name is the claim's own, so the
    // reason it gives is the holder's and replaces the one held.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = claimed_by_agent_one(&lab, home.path());
    let taken = claim_of(home.path(), "feat/gamma");

    let resumed = knives_at(
        &lab,
        home.path(),
        &workspace,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "finish the port",
        ],
    )
    .env("USER", "terminal-user")
    .output()
    .expect("resume from the workspace");

    assert!(
        resumed.status.success(),
        "derived resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let now = claim_of(home.path(), "feat/gamma");
    assert_eq!(now["why"], "finish the port");
    assert_eq!(now["owner"], "agent-one", "claim now: {now}");
    assert_eq!(
        now["kind"], taken["kind"],
        "the claim's identity source is not the resuming caller's: {now}"
    );
    assert_eq!(now["started"], taken["started"], "claim now: {now}");
    assert_eq!(now["place"], taken["place"], "claim now: {now}");
    let entry = claim_change(home.path(), "resumed");
    assert_eq!(
        entry.claim_change_why.as_deref(),
        Some("finish the port"),
        "the entry carries the reason the claim holds after it: {entry:?}"
    );
}

#[test]
fn start_refuses_two_anonymous_owners_with_the_same_name() {
    // Equal OS-user strings are not a trustworthy identity proof, so the second
    // anonymous terminal must receive the claim context and an explicit override.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let outside = tempfile::tempdir().expect("create unmanaged terminal");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(outside.path())
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env_remove("KNIVES_OWNER")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OMP_SESSION_ID")
            .env("USER", "terminal-user")
            .output()
            .expect("run start")
    };

    let first = run(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--why",
        "port it",
    ]);
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run(&["--text", "start", "feat/gamma", "--repo", "demo"]);

    assert_eq!(
        second.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    assert!(
        stderr.contains("both sides are anonymous identities"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("can never match"), "stderr: {stderr}");
    assert!(stderr.contains("port it"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!("its workspace is {}", workspace.display())),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("taken in this place"), "stderr: {stderr}");
    assert!(stderr.contains("harness session"), "stderr: {stderr}");
}

#[test]
fn start_refuses_another_harness_session_and_names_the_holder() {
    // A different harness identity must not inherit the first agent's workspace
    // silently; the refusal names enough context to make the override auditable.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_OWNER", owner)
            .output()
            .expect("run start")
    };

    let first = run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = run(
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    assert_eq!(
        second.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert!(stderr.contains("harness-session"), "stderr: {stderr}");
    assert!(stderr.contains("claimed"), "stderr: {stderr}");
    assert!(stderr.contains("last seen"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!("its workspace is {}", workspace.display())),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("no process of this machine"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("taken in this place"), "stderr: {stderr}");
    assert!(
        !stderr.contains("is evidence of nothing"),
        "a silent scan of this very place proves something: {stderr}"
    );
    assert!(stderr.contains("harness session"), "stderr: {stderr}");
}

/// `agent-one` claims `feat/gamma` with the reason every refusal test reads
/// back, and the workspace the claim opened.
fn claimed_by_agent_one(lab: &Lab, home: &Path) -> std::path::PathBuf {
    ran(
        lab,
        home,
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    lab.work.parent().expect("parent").join("feat-gamma")
}

#[test]
fn start_refusing_a_claim_whose_workspace_is_gone_says_nothing_is_there() {
    // An empty process list reads as an idle workspace. With the directory
    // itself gone there is nothing to be idle in, and only the refusal can
    // tell the reader which of the two it found.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = claimed_by_agent_one(&lab, home.path());
    std::fs::remove_dir_all(&workspace).expect("remove workspace directory");

    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "its workspace is {}, and nothing is at that path",
            workspace.display()
        )),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("pid "),
        "a process in a directory that is not there: {stderr}"
    );
}

#[test]
fn start_refusing_a_claim_names_the_processes_working_in_its_workspace() {
    // The fact that decides whether to seize a branch is whether anything is
    // still working in its workspace, and `knives finish` goes on saying
    // nothing of it: its refusal is about the claim alone.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = claimed_by_agent_one(&lab, home.path());
    // knives never counts the processes of the session calling it, so this
    // stand-in for the holder's shell carries none of this test's variables.
    let mut working = Command::new("sleep");
    working.arg("3600").current_dir(&workspace);
    for name in knives::commands::claim::HARNESS_SESSION_VARIABLES {
        working.env_remove(name);
    }
    let mut holder = working.spawn().expect("spawn a process in the workspace");

    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );
    let finished = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );
    let _ = holder.kill();
    let _ = holder.wait();

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!("pid {} (sleep 3600) at ", holder.id())),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("is evidence that the work"),
        "stderr: {stderr}"
    );

    let refused_finish = String::from_utf8_lossy(&finished.stderr);
    assert_eq!(finished.status.code(), Some(2), "stderr: {refused_finish}");
    assert!(
        refused_finish.contains("is claimed by agent-one"),
        "stderr: {refused_finish}"
    );
    for silent in [
        format!("pid {}", holder.id()),
        "its workspace is ".to_owned(),
        "taken in this place".to_owned(),
        "taken in another place".to_owned(),
        "records no place".to_owned(),
    ] {
        assert!(
            !refused_finish.contains(&silent),
            "the finish refusal gained {silent:?}: {refused_finish}"
        );
    }
}

/// Fail when `text` still holds a control character.
///
/// Asked of every character through [`char::is_control`], which is what the
/// escape itself asks (`inline_human_text`, src/ledger.rs), rather than of a
/// fixed list of the characters these tests plant: a list passes on every
/// control character nobody wrote down, the C1 range included. The line feed
/// is the one exception, because knives writes it itself to end a line.
fn free_of_control_characters(text: &str) {
    for (index, character) in text.char_indices() {
        assert!(
            !character.is_control() || character == '\n',
            "{character:?} reached the terminal at byte {index}: {text:?}"
        );
    }
}

#[test]
fn start_refusing_a_claim_escapes_the_control_characters_of_a_live_process() {
    // The command line of a process in the workspace is the text of whoever
    // started it, and this refusal prints every such process
    // (`claim_evidence`, src/commands/start.rs). The finish refusal's escape
    // of the same list is pinned in tests/finish_keeps_live_work.rs; this is
    // the start refusal's, and it drives a C1 introducer beside the C0 ones.
    use std::os::unix::process::CommandExt as _;

    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = claimed_by_agent_one(&lab, home.path());
    // knives never counts the processes of the session calling it, so this
    // stand-in for the holder's shell carries none of this test's variables.
    let mut working = Command::new("sleep");
    working
        .arg0("sleep\u{1b}]0;held\u{7}\u{9b}31m")
        .arg("3600")
        .current_dir(&workspace);
    for name in knives::commands::claim::HARNESS_SESSION_VARIABLES {
        working.env_remove(name);
    }
    let mut holder = working.spawn().expect("spawn a process in the workspace");

    // When: another identity tries to start the claimed branch
    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );
    let _ = holder.kill();
    let _ = holder.wait();

    // Then: the process is named with every control character of its command
    // line as U+FFFD
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr:?}");
    assert!(
        stderr.contains(&format!(
            "pid {} (sleep\u{fffd}]0;held\u{fffd}\u{fffd}31m 3600) at ",
            holder.id()
        )),
        "stderr: {stderr:?}"
    );
    free_of_control_characters(&stderr);
}

#[test]
fn start_refusing_a_claim_taken_in_another_place_says_the_scan_proves_nothing() {
    // The processes of another host or agent box are invisible from here, so
    // an empty list says nothing about a claim taken in one of them; a reader
    // who took it for an idle workspace would seize live work. The place is
    // the text of whatever session took the claim, so it reaches this
    // terminal escaped.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    claimed_by_agent_one(&lab, home.path());
    let state_path = home.path().join("state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).expect("state"))
            .expect("parse state");
    state["claims"]["demo/feat/gamma"]["place"] =
        Value::String("agent-box-7 \u{1b}]0;held\u{7} pid:[4026532999]".to_owned());
    std::fs::write(
        &state_path,
        serde_json::to_string(&state).expect("serialise state"),
    )
    .expect("rewrite state");

    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains(
            "taken in another place, agent-box-7 \u{fffd}]0;held\u{fffd} pid:[4026532999]"
        ),
        "stderr: {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "an escape character reached the terminal: {stderr:?}"
    );
    assert!(
        stderr.contains("no process of this machine"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("is evidence of nothing"),
        "stderr: {stderr}"
    );
}

#[test]
fn the_start_refusal_escapes_the_control_characters_of_the_claims_reason() {
    // The reason is the text of whatever session took the claim, and every
    // session of this machine reads the state file it sits in: unescaped,
    // the holder chooses what the refused caller's terminal receives.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port \u{1b}]0;held\u{7}it",
        ],
    );
    assert_eq!(
        claim_of(home.path(), "feat/gamma")["why"].as_str(),
        Some("port \u{1b}]0;held\u{7}it"),
        "the state file is the raw record: only the terminal rendering escapes"
    );

    // When: another identity tries to start the claimed branch
    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    // Then: every control character of the reason reached the terminal as
    // U+FFFD, and no escape character survived
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr:?}");
    assert!(
        stderr.contains("port \u{fffd}]0;held\u{fffd}it"),
        "stderr: {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "an escape character reached the terminal: {stderr:?}"
    );
}

/// Set the mode of the checkout's jj operation objects: how a test takes its
/// operation log away and gives it back.
fn operations_mode(checkout: &Path, mode: u32) {
    let operations = checkout
        .join(".jj")
        .join("repo")
        .join("op_store")
        .join("operations");
    std::fs::set_permissions(&operations, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Make the checkout's jj operation log unreadable, so opening the repository
/// fails where the claim decision never needed it; `false` when this user
/// reads it anyway (root ignores modes), and the caller has nothing to test.
fn unreadable_operation_log(checkout: &Path) -> bool {
    operations_mode(checkout, 0o000);
    if Repo::open(checkout).is_ok() {
        operations_mode(checkout, 0o755);
        return false;
    }
    true
}

/// Every sentence a refusal must not say when it read no window at all.
fn asserts_a_window(stderr: &str) -> bool {
    stderr.contains("last seen")
        || stderr.contains("observation window")
        || stderr.contains("no activity observed")
}

#[test]
fn start_refuses_a_held_claim_whose_operation_log_cannot_be_read() {
    // Who holds the branch is in the state file, so a checkout whose operation
    // log cannot be read must still be refused and told who holds it; the
    // sighting is the only thing the unreadable log costs.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    if !unreadable_operation_log(&lab.work) {
        return;
    }

    // When: another session starts the branch the unreadable checkout holds.
    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );
    operations_mode(&lab.work, 0o755);

    // Then: the claim refusal, with no sighting and no window it never read.
    assert_eq!(
        refused.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert!(stderr.contains("port it"), "stderr: {stderr}");
    assert!(
        stderr.contains("operation log could not be read"),
        "stderr: {stderr}"
    );
    assert!(!asserts_a_window(&stderr), "stderr: {stderr}");
}

#[test]
fn finish_refuses_a_held_claim_whose_operation_log_cannot_be_read() {
    // The same for the other verb: the holder is named, the refusal stands,
    // and nothing about the unreadable log reaches the exit code.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    if !unreadable_operation_log(&lab.work) {
        return;
    }

    // When: another session finishes the branch agent-one holds.
    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );
    operations_mode(&lab.work, 0o755);

    assert_eq!(
        refused.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert!(stderr.contains("port it"), "stderr: {stderr}");
    assert!(
        stderr.contains("operation log could not be read"),
        "stderr: {stderr}"
    );
    assert!(!asserts_a_window(&stderr), "stderr: {stderr}");
}

#[test]
fn start_from_inside_the_claimed_workspace_resumes_by_possession() {
    // Possession is intentionally weaker than a harness identity and must leave
    // its own ledger trail instead of mutating the held claim.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let first = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("first start");
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");

    let second = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "start", "feat/gamma", "--repo", "demo"])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env_remove("KNIVES_OWNER")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("OMP_SESSION_ID")
        .env("USER", "terminal-user")
        .output()
        .expect("resume from workspace");

    assert!(
        second.status.success(),
        "possession resume failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("possession"),
        "stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let events = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ])
        .current_dir(&workspace)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("read events");
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    assert!(
        String::from_utf8_lossy(&events.stdout).contains("resumed via workspace possession"),
        "events: {}",
        String::from_utf8_lossy(&events.stdout)
    );
}

#[test]
fn start_force_seizes_and_records_the_previous_owner() {
    // A force seizure preserves the workspace and records both the displaced
    // identity and the new reason in the durable event stream.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_OWNER", owner)
            .output()
            .expect("run start")
    };
    let first = run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    assert!(
        first.status.success(),
        "first start failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let change = lab.revision(&workspace, "@", "change_id.short(12)");

    let second = run(
        "agent-two",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );

    assert!(
        second.status.success(),
        "force start failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-two".to_owned())
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains(change.trim()),
        "stdout: {}",
        String::from_utf8_lossy(&second.stdout)
    );
    let events = run(
        "agent-two",
        &[
            "--text",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ],
    );
    assert!(
        events.status.success(),
        "read events failed: {}",
        String::from_utf8_lossy(&events.stderr)
    );
    let events = String::from_utf8_lossy(&events.stdout);
    assert!(
        events.contains("seized from agent-one (harness-session"),
        "events: {events}"
    );
    assert!(events.contains("rescue stalled work"), "events: {events}");
}

#[test]
fn start_adopts_an_existing_workspace_for_an_unclaimed_branch() {
    // A workspace made outside knives is still valid work to claim. The command
    // must reuse it rather than dying on the destination's existence.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &workspace, "main@upstream")
        .expect("create existing workspace");
    let change = lab.revision(&workspace, "@", "change_id.short(12)");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "adopt it",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("adopt workspace");

    assert!(
        output.status.success(),
        "adopt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("adopted"), "stdout: {stdout}");
    assert!(stdout.contains("left as-is"), "stdout: {stdout}");
    assert!(stdout.contains(change.trim()), "stdout: {stdout}");
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-one".to_owned())
    );
}

#[test]
fn start_adopts_a_no_cleanup_forgotten_workspace_without_resetting_it() {
    // `finish --no-cleanup` intentionally keeps the directory, but forgets its
    // registration. Starting it again must reattach that exact working copy.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run_start = |why: &str| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args([
                "--text",
                "start",
                "feat/gamma",
                "--repo",
                "demo",
                "--why",
                why,
            ])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_OWNER", "agent-one")
            .output()
            .expect("run start")
    };
    let started = run_start("port it");
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let work_file = workspace.join("in-progress.txt");
    std::fs::write(&work_file, "preserve this work\n").expect("write in-progress work");
    let change_before = lab.revision(&workspace, "@", "change_id");

    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--no-cleanup",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("finish without cleanup");
    assert!(
        finished.status.success(),
        "finish failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(workspace.is_dir(), "workspace directory was removed");

    let restarted = run_start("resume preserved work");

    assert!(
        restarted.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert!(
        String::from_utf8_lossy(&restarted.stdout).contains("adopted"),
        "stdout: {}",
        String::from_utf8_lossy(&restarted.stdout)
    );
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["demo/feat/gamma"]["owner"],
        Value::String("agent-one".to_owned())
    );
    assert_eq!(
        lab.revision(&workspace, "@", "change_id"),
        change_before,
        "adoption reset the working-copy change"
    );
    assert_eq!(
        std::fs::read_to_string(&work_file).expect("read preserved work"),
        "preserve this work\n"
    );
}

#[test]
fn start_refuses_a_same_named_workspace_from_another_repository() {
    // A sibling directory can belong to any jj repository. Reattachment is only
    // safe when its retained working-copy state belongs to the managed checkout.
    let lab = lab::Lab::new();
    let foreign = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&foreign.work, "feat-gamma", &workspace, "main@upstream")
        .expect("create foreign workspace at target path");
    let main_workspaces_before = Repo::open(&lab.work)
        .expect("open managed repo")
        .workspaces()
        .expect("list managed workspaces");
    let foreign_change_before = foreign.revision(&workspace, "@", "change_id");
    let main_operations_before = operation_ids(&lab.work);
    let foreign_operations_before = operation_ids(&foreign.work);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "do not seize foreign work",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("start against foreign workspace");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&lab.work.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&foreign.work.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(
        !home.path().join("state.json").exists(),
        "foreign workspace wrote a claim"
    );
    assert_eq!(
        Repo::open(&lab.work)
            .expect("reopen managed repo")
            .workspaces()
            .expect("list managed workspaces"),
        main_workspaces_before,
        "foreign workspace changed the managed repository"
    );
    assert_eq!(
        foreign.revision(&workspace, "@", "change_id"),
        foreign_change_before,
        "foreign workspace state changed"
    );
    assert_eq!(
        operation_ids(&lab.work),
        main_operations_before,
        "foreign workspace wrote a managed-repository operation"
    );
    assert_eq!(
        operation_ids(&foreign.work),
        foreign_operations_before,
        "foreign workspace wrote a foreign-repository operation"
    );
}

#[test]
fn start_resume_reports_a_missing_workspace_without_rebuilding_it() {
    // Given: the claim survives after its workspace directory is removed.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_OWNER", "agent-one")
            .output()
            .expect("run start")
    };
    let first = run(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--why",
        "port it",
    ]);
    assert!(first.status.success(), "{first:?}");
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    std::fs::remove_dir_all(&workspace).expect("remove workspace directory");

    // When: the claimant resumes without forcing a rebuild.
    let resumed = run(&["--text", "start", "feat/gamma", "--repo", "demo"]);

    // Then: resume remains an observation, not a workspace creation operation.
    assert!(resumed.status.success(), "resume failed: {resumed:?}");
    assert!(!workspace.exists(), "resume rebuilt the missing workspace");
    let stdout = String::from_utf8_lossy(&resumed.stdout);
    assert!(stdout.contains("workspace missing"), "stdout: {stdout}");
    assert!(stdout.contains("--force"), "stdout: {stdout}");
}

#[test]
fn force_claim_does_not_save_state_when_its_provenance_cannot_be_appended() {
    // Given: a held claim and a ledger directory the store can read but no
    // entry can be written into.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_OWNER", owner)
            .output()
            .expect("run start")
    };
    let first = run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    assert!(first.status.success(), "{first:?}");
    let state_before = std::fs::read(home.path().join("state.json")).expect("read state");
    // Block the append, not the read: the store reads the fork's ledger when
    // it opens, and a ledger it cannot read would stop the command there,
    // before the append this test is about.
    let ledger = home.path().join("ledger").join("demo");
    if !unwritable(&ledger) {
        return;
    }

    // When: another owner forces the claim but its provenance write fails.
    let forced = run(
        "agent-two",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Then: the append is what failed, and the forced claim never becomes
    // current without its event.
    assert!(
        !forced.status.success(),
        "force unexpectedly succeeded: {forced:?}"
    );
    let stderr = String::from_utf8_lossy(&forced.stderr);
    assert!(
        stderr.contains(&format!("writing {}", ledger.display())),
        "the command failed somewhere other than the append: {stderr}"
    );
    assert_eq!(
        std::fs::read(home.path().join("state.json")).expect("read state"),
        state_before,
        "ledger failure saved an unprovenanced forced claim"
    );
}

#[test]
fn force_finish_does_not_save_state_when_its_provenance_cannot_be_appended() {
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let start = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("start claim");
    assert!(start.status.success(), "{start:?}");
    let state_before = std::fs::read(home.path().join("state.json")).expect("read state");
    // Block the append, not the read, as above.
    let ledger = home.path().join("ledger").join("demo");
    if !unwritable(&ledger) {
        return;
    }

    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--repo",
            "demo",
            "--no-cleanup",
            "--force",
            "--why",
            "release stalled work",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-two")
        .output()
        .expect("force finish");
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    assert!(
        !finished.status.success(),
        "finish unexpectedly succeeded: {finished:?}"
    );
    let stderr = String::from_utf8_lossy(&finished.stderr);
    assert!(
        stderr.contains(&format!("writing {}", ledger.display())),
        "the command failed somewhere other than the append: {stderr}"
    );
    assert_eq!(
        std::fs::read(home.path().join("state.json")).expect("read state"),
        state_before,
        "ledger failure saved an unprovenanced forced finish"
    );
}

#[test]
fn forced_finish_with_supersession_writes_one_atomic_provenance_event() {
    // Given: another owner holds the claim that this terminal must force-release.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let start = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("start claim");
    assert!(start.status.success(), "{start:?}");

    // When: the claim is force-finished while recording where its work continued.
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--repo",
            "demo",
            "--no-cleanup",
            "--force",
            "--why",
            "release stalled work",
            "--superseded-by",
            "feat/replacement",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-two")
        .output()
        .expect("force finish");

    // Then: one durable event proves both facts, so no second append can strand a false release.
    assert!(finished.status.success(), "{finished:?}");
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .read()
        .expect("read ledger")
        .entries;
    assert_eq!(entries.len(), 2, "was: {entries:?}");
    let event = &entries[1].text;
    assert!(
        event.contains("released agent-one's claim by force"),
        "event: {event}"
    );
    assert!(event.contains("release stalled work"), "event: {event}");
    assert!(
        event.contains("superseded by feat/replacement"),
        "event: {event}"
    );
    assert_eq!(
        entries[1].statement,
        Some(knives::statement::Statement {
            kind: knives::statement::StatementKind::Superseded,
            value: Some("feat/replacement".to_owned()),
        }),
        "the forced release's one event carries the supersession"
    );
}

#[test]
fn start_refuses_a_forgotten_same_repo_workspace_with_a_different_name_before_reattaching() {
    // Given: a forgotten workspace from this repository occupies another branch's destination.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-other", &workspace, "main@upstream")
        .expect("create other branch workspace");
    knives::jj::forget_workspace(&lab.work, "feat-other").expect("forget other workspace");
    let operations_before = operation_ids(&lab.work);

    // When: feat/gamma attempts to adopt that path.
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "do not seize other branch work",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("start against another branch workspace");

    // Then: both names are disclosed and no reattachment transaction ran.
    assert_eq!(output.status.code(), Some(2), "output: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("feat-other"), "stderr: {stderr}");
    assert!(stderr.contains("feat-gamma"), "stderr: {stderr}");
    assert_eq!(
        operation_ids(&lab.work),
        operations_before,
        "wrong-name workspace was reattached before refusal"
    );
}

#[test]
fn start_refuses_a_malformed_foreign_workspace_before_loading_it() {
    // Identity comes from `.jj/repo`; a broken foreign working-copy state must
    // not turn that clear mismatch into an incomplete-command error.
    let lab = lab::Lab::new();
    let foreign = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&foreign.work, "feat-gamma", &workspace, "main@upstream")
        .expect("create foreign workspace at target path");
    std::fs::write(
        workspace.join(".jj/working_copy/checkout"),
        "not a working-copy state",
    )
    .expect("corrupt foreign working-copy state");
    let main_operations_before = operation_ids(&lab.work);
    let foreign_operations_before = operation_ids(&foreign.work);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "do not load foreign work",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("start against malformed foreign workspace");

    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&lab.work.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&foreign.work.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(
        !home.path().join("state.json").exists(),
        "malformed foreign workspace wrote a claim"
    );
    assert_eq!(operation_ids(&lab.work), main_operations_before);
    assert_eq!(operation_ids(&foreign.work), foreign_operations_before);
}

#[test]
fn start_force_without_why_is_a_usage_error() {
    // Clap owns this validation so a force never reaches claim handling without
    // a durable human explanation.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "start", "feat/gamma", "--repo", "demo", "--force"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .output()
        .expect("parse start");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_dispatch_records_an_observation_before_running_the_command() {
    let lab = Lab::new();
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"o\"\n",
            lab.upstream.display()
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "repos"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("run knives");

    assert!(
        output.status.success(),
        "knives failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("seen.json"))
            .expect("CLI dispatch records seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["owners"]["harness-session"]["agent-one"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
    assert!(
        seen["workspaces"]["demo/work"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
}

#[test]
fn claim_events_name_what_happened_to_the_holder_in_fields() {
    // A tool asking who holds a branch reads fields, never prose: an entry's
    // sentence is written for a person and free to be reworded. A field no
    // write set is absent, so a reader can tell "nothing displaced" from
    // "displaced someone unnamed".
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| ran(&lab, home.path(), owner, args);

    run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    run(
        "agent-one",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );
    let held: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    let started = held["claims"]["demo/feat/gamma"]["started"].clone();
    run(
        "agent-two",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );
    run(
        "agent-two",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let events = run(
        "agent-two",
        &[
            "--json",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ],
    );
    let read: Value = serde_json::from_slice(&events.stdout).expect("events JSON");
    let entries = read["entries"].as_array().expect("entries").clone();

    let claimed = entry_starting(&entries, "claimed: port it");
    assert_eq!(claimed["claim_change"], "claimed");
    assert_eq!(claimed["claim_change_why"], "port it");
    assert!(
        claimed.get("previous_owner").is_none(),
        "a first claim displaced nobody: {claimed}"
    );

    let resumed = entry_starting(&entries, "resumed");
    assert_eq!(resumed["claim_change"], "resumed");
    assert_eq!(resumed["claim_change_why"], "port it");
    assert!(
        resumed.get("previous_owner").is_none(),
        "a resume displaced nobody: {resumed}"
    );

    let taken = entry_starting(&entries, "seized from agent-one");
    assert_eq!(taken["claim_change"], "taken");
    assert_eq!(taken["claim_change_why"], "rescue stalled work");
    assert_eq!(taken["previous_owner"], "agent-one");
    assert_eq!(taken["previous_kind"], "harness-session");
    assert_eq!(taken["previous_started"], started);
    assert_eq!(taken["previous_why"], "port it");
    assert!(
        durable_sighting(&taken["previous_last_seen"]),
        "was: {taken}"
    );

    let released = entry_starting(&entries, "claim released");
    assert_eq!(released["claim_change"], "released");
    assert!(
        released.get("claim_change_why").is_none(),
        "an ordinary release has no reason to give: {released}"
    );
    assert!(
        released.get("previous_owner").is_none(),
        "a release of one's own claim displaced nobody: {released}"
    );
}

#[test]
fn a_forced_release_records_the_claim_it_ended() {
    // The agent whose claim another agent released reads it here: the one
    // entry that ended the claim names who held it, since when, when it was
    // last seen working and why it was held at all.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| ran(&lab, home.path(), owner, args);

    run(
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    let held: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("state"),
    )
    .expect("parse state");
    let started = held["claims"]["demo/feat/gamma"]["started"].clone();
    run(
        "agent-two",
        &[
            "--text",
            "finish",
            "feat/gamma",
            "--repo",
            "demo",
            "--no-cleanup",
            "--force",
            "--why",
            "release stalled work",
        ],
    );

    let events = run(
        "agent-two",
        &[
            "--json",
            "notch",
            "feat/gamma",
            "--events",
            "--repo",
            "demo",
        ],
    );
    let read: Value = serde_json::from_slice(&events.stdout).expect("events JSON");
    let entries = read["entries"].as_array().expect("entries").clone();

    let released = entry_starting(&entries, "released agent-one's claim by force");
    assert_eq!(released["claim_change"], "released");
    assert_eq!(released["claim_change_why"], "release stalled work");
    assert_eq!(released["previous_owner"], "agent-one");
    assert_eq!(released["previous_kind"], "harness-session");
    assert_eq!(released["previous_started"], started);
    assert_eq!(released["previous_why"], "port it");
    assert!(
        durable_sighting(&released["previous_last_seen"]),
        "was: {released}"
    );
}

/// `agent-one` claims `feat/gamma` and `agent-two` takes it by force: the
/// chain every displaced-owner test starts from, and the directory the
/// takeover left in place.
fn taken_over(lab: &Lab, home: &Path) -> std::path::PathBuf {
    ran(
        lab,
        home,
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    ran(
        lab,
        home,
        "agent-two",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );
    lab.work.parent().expect("parent").join("feat-gamma")
}

#[test]
fn a_displaced_owners_finish_names_the_takeover_and_offers_no_release() {
    // Measured with v3.2.4: the refusal named the taker as if it were the
    // caller's own claim, said nothing of the claim this caller lost, and
    // offered a `finish --force` that releases the taker's live claim and
    // leaves the branch claimed by nobody.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = taken_over(&lab, home.path());
    let taken = claim_change(home.path(), "taken");
    let entries = ledger_of(home.path()).len();

    // When: the owner that lost the claim finishes the branch
    let told = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: it learns who took its claim, when, why, and who holds the
    // branch now, and is told how to take it back rather than how to release
    // a claim that is not its own. Nothing moved.
    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "agent-two took agent-one's claim on demo/feat/gamma at {}: rescue stalled work",
            taken.ts
        )),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("is claimed by agent-two"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("use `knives start feat/gamma --force --why"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("knives finish feat/gamma --force"),
        "the release of the holder's live claim was offered: {stderr}"
    );
    assert_eq!(
        holder_of(home.path(), "feat/gamma").as_deref(),
        Some("agent-two")
    );
    assert!(workspace.is_dir(), "the holder's workspace was removed");
    assert_eq!(
        ledger_of(home.path()).len(),
        entries,
        "a finish that changed nothing wrote to the ledger"
    );
}

#[test]
fn the_takeover_notice_escapes_the_control_characters_of_the_ledger() {
    // A ledger entry can come from another machine through the ledger pull,
    // and the entry that ends a claim names the owner it displaced: without
    // escaping, whoever wrote it chooses what the displaced owner's terminal
    // receives the next time that owner runs `knives finish`.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    ran(
        &lab,
        home.path(),
        "agent-two",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue \u{1b}]0;seized\u{7}stalled work",
        ],
    );
    assert_eq!(
        claim_change(home.path(), "taken")
            .claim_change_why
            .as_deref(),
        Some("rescue \u{1b}]0;seized\u{7}stalled work"),
        "the ledger is the raw record: only the terminal rendering escapes"
    );
    // The taker hands the branch back, so nobody holds it and the notice the
    // displaced owner gets is read from the ledger alone.
    ran(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // When: the owner that lost the claim finishes the branch
    let told = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: every control character of the reason reached the terminal as
    // U+FFFD, and no escape sequence survived
    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr:?}");
    assert!(
        stderr.contains("rescue \u{fffd}]0;seized\u{fffd}stalled work"),
        "stderr: {stderr:?}"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "an escape character reached the terminal: {stderr:?}"
    );
}

/// An owner name holding the control characters of an OSC sequence.
///
/// `KNIVES_OWNER` refuses only a blank name, so a claimant can hold this
/// one; a session with no harness variable set reads its own name back out
/// of a claim in the state file, so an owner name is another party's text
/// wherever this tool prints one.
const HOSTILE_OWNER: &str = "agent-\u{1b}]0;x\u{7}\u{9b}31m\u{9d}2;y-one";

/// [`HOSTILE_OWNER`] as every terminal must read it. Its C1 controls are
/// control characters a terminal reading UTF-8 as ISO 2022 acts on — U+009B
/// is CSI and U+009D is OSC — so they escape as the C0 ones do.
const ESCAPED_OWNER: &str = "agent-\u{fffd}]0;x\u{fffd}\u{fffd}31m\u{fffd}2;y-one";

#[test]
fn an_owner_name_holding_control_characters_reaches_every_terminal_escaped() {
    // Given: that name holds the claim on the branch
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let claimed = ran(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    assert_eq!(
        claim_of(home.path(), "feat/gamma")["owner"].as_str(),
        Some(HOSTILE_OWNER),
        "the state file is the raw record: only the terminal rendering escapes"
    );
    let stdout = String::from_utf8_lossy(&claimed.stdout);
    assert!(
        stdout.contains(&format!("claimed demo/feat/gamma for {ESCAPED_OWNER}")),
        "stdout: {stdout:?}"
    );
    free_of_control_characters(&stdout);

    // When: another owner is refused the branch
    let refused = tried(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    );

    // Then: the refusal names the holder with each control as U+FFFD
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr:?}");
    assert!(stderr.contains(ESCAPED_OWNER), "stderr: {stderr:?}");
    free_of_control_characters(&stderr);

    // When: that owner forces the release of the claim
    let forced = ran(
        &lab,
        home.path(),
        "agent-two",
        &[
            "--text",
            "finish",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "owner session died",
        ],
    );

    // Then: the line that leaves the displaced owner's workspace alone names
    // that owner escaped
    let stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(
        stdout.contains(&format!("the directory is {ESCAPED_OWNER}'s")),
        "stdout: {stdout:?}"
    );
    free_of_control_characters(&stdout);

    // When: the displaced owner finishes the branch it believed it held
    let told = tried(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the notice it gets names it escaped too
    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr:?}");
    assert!(
        stderr.contains(&format!(
            "released {ESCAPED_OWNER}'s claim on demo/feat/gamma"
        )),
        "stderr: {stderr:?}"
    );
    free_of_control_characters(&stderr);

    // When: the branch is started again with a workspace directory already at
    // the destination — the finish above disposed of the one this flow left,
    // so the directory is made here as a workspace made outside knives is
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    knives::jj::add_workspace(&lab.work, "feat-gamma", &workspace, "main@upstream")
        .expect("create existing workspace");
    let adopted = ran(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "take it back",
        ],
    );

    // Then: the adoption path's `claimed … for` line names it escaped, as the
    // fresh workspace's line did at the top of this test: both lines print
    // the one escaped owner `take_claim` binds.
    let stdout = String::from_utf8_lossy(&adopted.stdout);
    assert!(
        stdout.contains("adopted existing workspace"),
        "stdout: {stdout:?}"
    );
    assert!(
        stdout.contains(&format!("claimed demo/feat/gamma for {ESCAPED_OWNER}")),
        "stdout: {stdout:?}"
    );
    free_of_control_characters(&stdout);
}

#[test]
fn a_takeover_by_an_owner_holding_control_characters_names_that_owner_escaped() {
    // The notice a displaced owner gets names two owners: this run's own
    // identity, which the test above drives hostile, and the owner the ledger
    // entry records as the taker (`lost.owner`, `displacement` in
    // src/branch_verbs.rs). The ledger pull brings entries from other
    // machines, so that name is another party's text wherever it is read, and
    // only a hostile taker drives it.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    ran(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );
    assert_eq!(
        claim_change(home.path(), "taken").owner,
        HOSTILE_OWNER,
        "the ledger is the raw record: only the terminal rendering escapes"
    );
    // The taker hands the branch back, so nobody holds it and the notice the
    // displaced owner gets is read from the ledger alone.
    ran(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // When: the owner that lost the claim finishes the branch
    let told = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the taker the entry names reaches this terminal escaped
    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr:?}");
    assert!(
        stderr.contains(&format!(
            "{ESCAPED_OWNER} took agent-one's claim on demo/feat/gamma at "
        )),
        "stderr: {stderr:?}"
    );
    free_of_control_characters(&stderr);
}

#[test]
fn a_finish_from_another_place_names_the_holding_owner_escaped() {
    // A claim taken in another host or agent box refuses this finish and names
    // the owner that holds it (`place_gate`, src/branch_verbs.rs). That owner
    // is the text of whatever session took the claim, in a state file every
    // session of this machine reads; the place's own escape is pinned in
    // tests/finish_keeps_live_work.rs, and this pins the owner's.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    let path = home.path().join("state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read state"))
            .expect("parse state");
    assert!(
        state["claims"]["demo/feat/gamma"]["place"].is_string(),
        "start recorded no place: {state}"
    );
    state["claims"]["demo/feat/gamma"]["place"] = Value::String("agent-box-7".to_owned());
    std::fs::write(&path, state.to_string()).expect("write state");

    // When: the owner that holds it finishes from this place
    let refused = tried(
        &lab,
        home.path(),
        HOSTILE_OWNER,
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    // Then: the refusal names the holder with every control character as
    // U+FFFD, and the claim stands
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr:?}");
    assert!(
        stderr.contains(&format!("was claimed by {ESCAPED_OWNER} in agent-box-7;")),
        "stderr: {stderr:?}"
    );
    free_of_control_characters(&stderr);
    assert_eq!(
        holder_of(home.path(), "feat/gamma").as_deref(),
        Some(HOSTILE_OWNER)
    );
}

#[test]
fn a_displaced_owner_standing_in_the_lost_workspace_is_told_the_same() {
    // Possession of the directory is how an agent runs `finish` at all, and
    // after a takeover the directory is the new holder's: the caller that
    // stands in it must learn what happened, not release the claim it is
    // standing on.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = taken_over(&lab, home.path());
    let taken = claim_change(home.path(), "taken");

    let told = lab::knives_command(
        &workspace,
        home.path(),
        lab.temp_path(),
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    )
    .env("KNIVES_OWNER", "agent-one")
    .output()
    .expect("finish from inside the lost workspace");

    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "agent-two took agent-one's claim on demo/feat/gamma at {}: rescue stalled work",
            taken.ts
        )),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("knives finish feat/gamma --force"),
        "the release of the holder's live claim was offered: {stderr}"
    );
    assert_eq!(
        holder_of(home.path(), "feat/gamma").as_deref(),
        Some("agent-two")
    );
    assert!(workspace.is_dir(), "the holder's workspace was removed");
}

#[test]
fn a_displaced_owners_finish_disposes_of_the_workspace_nobody_holds() {
    // The claim was released, not taken: there is no holder to protect, so
    // the caller is told what happened and its own workspace is disposed of
    // in the same run.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    ran(
        &lab,
        home.path(),
        "agent-two",
        &[
            "--text",
            "finish",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "release stalled work",
        ],
    );
    let released = claim_change(home.path(), "released");

    let told = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&told.stderr);
    let stdout = String::from_utf8_lossy(&told.stdout);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "agent-two released agent-one's claim on demo/feat/gamma at {}: release stalled work",
            released.ts
        )),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("nobody holds demo/feat/gamma now"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("use `knives start feat/gamma` to claim it again"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("--force --why"),
        "a forced command was offered where nobody holds the branch: {stderr}"
    );
    assert!(
        stdout.contains("workspace feat-gamma forgotten"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !workspace.exists(),
        "the caller's own workspace was not disposed of"
    );
}

#[test]
fn a_displaced_owners_finish_names_the_takeover_after_the_taker_finished() {
    // The entry that ended this caller's claim is the takeover, however many
    // releases of someone else's claim came after it.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = taken_over(&lab, home.path());
    let taken = claim_change(home.path(), "taken");
    ran(
        &lab,
        home.path(),
        "agent-two",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let told = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "agent-two took agent-one's claim on demo/feat/gamma at {}: rescue stalled work",
            taken.ts
        )),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("nobody holds demo/feat/gamma now"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("use `knives start feat/gamma` to claim it again"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_chapter_the_owner_closed_itself_ends_the_search() {
    // The owner took the branch up again after the takeover and handed it
    // back itself. A later finish of the unheld branch must say nothing of a
    // takeover two chapters old.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = taken_over(&lab, home.path());
    let finish = ["--text", "finish", "feat/gamma", "--repo", "demo"];
    ran(&lab, home.path(), "agent-two", &finish);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "take it up again",
        ],
    );
    ran(&lab, home.path(), "agent-one", &finish);

    // When: the owner finishes the branch it already handed back
    let again = tried(&lab, home.path(), "agent-one", &finish);

    // Then: an ordinary finish of nothing
    let stderr = String::from_utf8_lossy(&again.stderr);
    let stdout = String::from_utf8_lossy(&again.stdout);
    assert_eq!(again.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("was not held"), "stdout: {stdout}");
    assert!(
        !stderr.contains("agent-one's claim"),
        "a takeover the owner already answered was reported: {stderr}"
    );
}

#[test]
fn a_resume_in_the_lost_directory_does_not_hide_the_takeover() {
    // A displaced agent goes on working where it stands, and its resume
    // records no claim of its own: the takeover is still what ended its
    // claim, and still what its finish must be told.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = taken_over(&lab, home.path());
    let taken = claim_change(home.path(), "taken");
    let resumed = lab::knives_command(
        &workspace,
        home.path(),
        lab.temp_path(),
        &["--text", "start", "feat/gamma", "--repo", "demo"],
    )
    .env("KNIVES_OWNER", "agent-one")
    .output()
    .expect("resume in the lost workspace");
    assert!(
        resumed.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(claim_change(home.path(), "resumed").owner, "agent-one");

    let told = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&told.stderr);
    assert_eq!(told.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "agent-two took agent-one's claim on demo/feat/gamma at {}: rescue stalled work",
            taken.ts
        )),
        "stderr: {stderr}"
    );
}

#[test]
fn a_caller_that_never_held_the_branch_keeps_the_refusal_it_always_got() {
    // Releasing the claim of an agent that is gone is what the forced finish
    // is for, and a caller the ledger never names as a displaced owner must
    // still be told so.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = taken_over(&lab, home.path());

    let refused = tried(
        &lab,
        home.path(),
        "agent-three",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("is claimed by agent-two"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("use `knives finish feat/gamma --force --why"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("agent-three's claim"),
        "a claim this caller never held was reported: {stderr}"
    );
}

#[test]
fn an_owner_that_took_its_own_branch_back_is_told_of_no_takeover() {
    // The ledger holds a takeover of this caller's claim, and the caller took
    // the branch back after it: it holds the claim now, and the refusal is
    // the one two identities of a single OS-user name always get. Measured
    // before the fix: `agent-z took sami's claim` at exit 1, told to the
    // owner of the live claim.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let outside = tempfile::tempdir().expect("create unmanaged terminal");
    let as_sami = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(outside.path())
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env_remove("KNIVES_OWNER")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("OMP_SESSION_ID")
            .env("USER", "sami")
            .output()
            .expect("run knives as the OS user sami")
    };
    let claimed = as_sami(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--why",
        "port it",
    ]);
    assert!(
        claimed.status.success(),
        "sami's claim failed: {}",
        String::from_utf8_lossy(&claimed.stderr)
    );
    ran(
        &lab,
        home.path(),
        "agent-z",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--force",
            "--why",
            "rescue stalled work",
        ],
    );
    let back = as_sami(&[
        "--text",
        "start",
        "feat/gamma",
        "--repo",
        "demo",
        "--force",
        "--why",
        "the port is mine to finish",
    ]);
    assert!(
        back.status.success(),
        "taking the branch back failed: {}",
        String::from_utf8_lossy(&back.stderr)
    );
    assert_eq!(
        holder_of(home.path(), "feat/gamma").as_deref(),
        Some("sami")
    );

    // When: that owner finishes the branch from outside its workspace, with
    // no harness variable to tell the two identities of `sami` apart
    let refused = as_sami(&["--text", "finish", "feat/gamma", "--repo", "demo"]);

    // Then: the refusal two anonymous identities always get, and no takeover
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("both sides are anonymous identities"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("use `knives finish feat/gamma --force --why"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("sami's claim on"),
        "the holder was told its own claim was taken: {stderr}"
    );
    assert!(
        !stderr.contains("agent-z"),
        "a takeover the caller already answered was reported: {stderr}"
    );
    assert_eq!(
        holder_of(home.path(), "feat/gamma").as_deref(),
        Some("sami")
    );
}

#[test]
fn a_ledger_without_claim_fields_keeps_the_refusal_it_always_gave() {
    // Every entry an older knives wrote carries no claim change, and prose
    // is never read for one: such a ledger answers nothing, and the caller
    // gets the refusal it got before the fields existed.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let _workspace = taken_over(&lab, home.path());
    without_claim_fields(home.path());
    assert!(
        ledger_of(home.path())
            .iter()
            .all(|entry| entry.claim_change.is_none() && entry.previous_owner.is_none()),
        "the fixture left claim fields behind"
    );

    let refused = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stderr.contains("is claimed by agent-two"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("use `knives finish feat/gamma --force --why"),
        "stderr: {stderr}"
    );
}

#[test]
fn a_fork_ledger_that_cannot_be_read_fails_the_finish_naming_it_once() {
    // The finish fails before it reads a workspace or touches a claim, and
    // the one line it prints says which file could not be read and why,
    // once: the reason used to arrive twice, and never as the ledger's.
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    ran(
        &lab,
        home.path(),
        "agent-one",
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    );
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    let ledger = home.path().join("ledger").join("demo");
    if !unreadable(&ledger) {
        return;
    }

    let failed = tried(
        &lab,
        home.path(),
        "agent-one",
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    );
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert_eq!(failed.status.code(), Some(3), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!(
            "reading the fork's ledger at {}",
            ledger.display()
        )),
        "stderr: {stderr}"
    );
    assert_eq!(
        stderr.matches("Permission denied").count(),
        1,
        "the reason is printed more than once: {stderr}"
    );
    assert_eq!(
        holder_of(home.path(), "feat/gamma").as_deref(),
        Some("agent-one"),
        "the claim changed under an unreadable ledger"
    );
    assert!(workspace.is_dir(), "the workspace was disposed of");
}
