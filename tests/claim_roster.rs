//! A claim is kept under its fork's upstream name, and every reader of the
//! roster finds it there.
//!
//! The lab's other forks have a filesystem path for an upstream, which names
//! no forge repository, so they are kept under their registry key and a reader
//! that looked a claim up by that key would pass. These forks have a forge URL
//! for an upstream, as every real fork does: the claim is kept under the URL's
//! lowercase `<owner>/<name>`, and a reader that still asked by the registry
//! key would find an empty roster — `status` showing no claims, and the hook
//! telling an agent that nobody holds the branch it is about to take.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use lab::Lab;
use serde_json::{Value, json};

/// The fork's upstream as its registry spells it, letter case and all.
const UPSTREAM: &str = "https://forge.invalid/Maintainer/Demo";
/// What the fork's claims are kept under: [`UPSTREAM`]'s lowercase `<owner>/<name>`.
const KEPT_UNDER: &str = "maintainer/demo";

/// A lab whose work checkout's `upstream` is [`UPSTREAM`], and a config home
/// whose registry calls that fork `demo`.
fn forge_fork() -> (Lab, tempfile::TempDir) {
    let lab = Lab::new();
    lab.upstream_at_forge_url(UPSTREAM);
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!("[repos.demo]\nupstream = \"{UPSTREAM}\"\norigin = \"https://forge.invalid/acme/work.git\"\n"),
    )
    .expect("write registry");
    (lab, home)
}

/// `knives <args>` from `cwd` as harness session `owner`, or as the terminal
/// user `terminal-user` when `owner` is `None`.
#[allow(
    clippy::too_many_arguments,
    reason = "a fixture: which lab, which config home, where it runs, who runs it and what it runs are independent"
)]
fn knives_as(lab: &Lab, home: &Path, cwd: &Path, owner: Option<&str>, args: &[&str]) -> Output {
    let mut command = lab::knives_command(cwd, home, lab.temp_path(), args);
    command.env("USER", "terminal-user");
    if let Some(owner) = owner {
        command.env("KNIVES_OWNER", owner);
    }
    command.output().expect("run knives")
}

fn succeeded(output: &Output) -> &Output {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// `agent-one` claims `feat/gamma` on `demo` and gets its workspace.
fn claim_gamma(lab: &Lab, home: &Path) {
    succeeded(&knives_as(
        lab,
        home,
        &lab.work,
        Some("agent-one"),
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "port it",
        ],
    ));
}

/// The context Claude Code's `SessionStart` hook adds for a session started in `cwd`.
fn session_start_notice(home: &Path, cwd: &Path) -> String {
    let event = json!({
        "session_id": "hook-session",
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["hook", "claude-code"])
        .env("KNIVES_CONFIG_HOME", home)
        .env("HOME", home)
        .env("JJ_CONFIG", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(event.to_string().as_bytes())
        .expect("write hook input");
    let output = child.wait_with_output().expect("wait for hook");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "the hook emits JSON ({error}): {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additional context")
        .to_owned()
}

#[test]
fn a_claim_is_kept_under_the_forks_lowercase_upstream_name() {
    // Given: a fork whose upstream is a forge URL.
    let (lab, home) = forge_fork();

    // When: a branch of it is claimed.
    claim_gamma(&lab, home.path());

    // Then: the state file keeps the claim under the upstream name, not the
    // registry key: what every reader below must look it up by.
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("state JSON");
    let claim = &state["claims"][format!("{KEPT_UNDER}/feat/gamma")];
    assert_eq!(claim["repo"], KEPT_UNDER, "state: {state}");
    assert_eq!(claim["owner"], "agent-one", "state: {state}");
}

#[test]
fn a_claimed_branch_is_on_the_status_roster() {
    // Given: a claimed branch of a fork whose upstream is a forge URL.
    let (lab, home) = forge_fork();
    claim_gamma(&lab, home.path());

    // When: status reports the fork.
    let status = succeeded(&knives_as(
        &lab,
        home.path(),
        &lab.work,
        None,
        &["--json", "status", "demo", "--no-github", "--no-landed"],
    ))
    .stdout
    .clone();

    // Then: the branch's row names who holds it.
    let report: Value = serde_json::from_slice(&status).expect("status JSON");
    let row = report["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .find(|row| row["name"] == "feat/gamma")
        .unwrap_or_else(|| panic!("no feat/gamma row: {report}"));
    assert_eq!(row["claim"]["id"], "agent-one", "row: {row}");
    assert_eq!(row["claim"]["why"], "port it", "row: {row}");
}

#[test]
fn the_claude_code_notice_names_the_claimed_branch() {
    // Given: a claimed branch of a fork whose upstream is a forge URL.
    let (lab, home) = forge_fork();
    claim_gamma(&lab, home.path());

    // When: a Claude Code session starts in the fork.
    let notice = session_start_notice(home.path(), &lab.work);

    // Then: the notice names the claim instead of saying nobody holds a branch.
    assert!(
        notice.contains("feat/gamma (agent-one, harness-session, claimed "),
        "notice: {notice}"
    );
    assert!(
        !notice.contains("No branch is claimed here right now."),
        "notice: {notice}"
    );
}

#[test]
fn a_terminal_in_the_fork_acts_as_its_sole_claimant() {
    // Given: a fork whose only claim is agent-one's, held as a harness session.
    let (lab, home) = forge_fork();
    claim_gamma(&lab, home.path());

    // When: a terminal with no harness session writes a note from inside the fork.
    succeeded(&knives_as(
        &lab,
        home.path(),
        &lab.work,
        None,
        &[
            "--text",
            "notch",
            "-m",
            "from the terminal",
            "--repo",
            "demo",
        ],
    ));

    // Then: the note is written as the fork's sole claimant, not the OS user.
    let read = succeeded(&knives_as(
        &lab,
        home.path(),
        &lab.work,
        None,
        &["--json", "notch", "--repo", "demo"],
    ))
    .stdout
    .clone();
    let notches: Value = serde_json::from_slice(&read).expect("notch JSON");
    let note = notches["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|entry| entry["text"] == "from the terminal")
        .unwrap_or_else(|| panic!("no note: {notches}"));
    assert_eq!(note["owner"], "agent-one", "note: {note}");
}

#[test]
fn a_sighting_inside_the_claimed_workspace_reaches_the_notice() {
    // Given: a claimed branch with every earlier sighting forgotten, so the
    // only evidence of activity is the one made below.
    let (lab, home) = forge_fork();
    claim_gamma(&lab, home.path());
    std::fs::remove_file(home.path().join("seen.json")).expect("forget earlier sightings");

    // When: a terminal runs knives inside the claim's workspace, which
    // records a sighting of that workspace.
    let workspace = lab.temp_path().join("feat-gamma");
    succeeded(&knives_as(
        &lab,
        home.path(),
        &workspace,
        None,
        &["--text", "repos"],
    ));

    // Then: the sighting is keyed where the notice looks for the claim's
    // workspace, so the claim reads as seen.
    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("seen.json")).expect("read seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["workspaces"][format!("{KEPT_UNDER}/feat-gamma")].is_string(),
        "seen: {seen}"
    );
    let notice = session_start_notice(home.path(), &lab.work);
    assert!(
        notice.contains("feat/gamma (agent-one, harness-session, claimed ")
            && notice.contains(" ago, last seen "),
        "notice: {notice}"
    );
}

#[test]
fn a_claims_repo_finds_its_checkout_by_the_repos_listings_upstream_name() {
    // Given: a claimed branch of a fork whose registry key is not its
    // upstream name, beside a second fork with a filesystem upstream.
    let (lab, home) = forge_fork();
    let registry = home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read registry");
    std::fs::write(
        &registry,
        format!(
            "{text}\n[repos.local]\nupstream = \"/srv/local-upstream\"\n\
             origin = \"https://forge.invalid/acme/local.git\"\n"
        ),
    )
    .expect("write registry");
    claim_gamma(&lab, home.path());

    // When: a consumer reads the claim from the state file, as one holding
    // only `state.json` does, and lists the managed forks.
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("state JSON");
    let claimed = state["claims"]
        .as_object()
        .expect("claims")
        .values()
        .next()
        .expect("one claim")["repo"]
        .as_str()
        .expect("a claim's repo")
        .to_owned();
    let listing = succeeded(&knives_as(
        &lab,
        home.path(),
        &lab.work,
        None,
        &["--json", "repos"],
    ))
    .stdout
    .clone();
    let report: Value = serde_json::from_slice(&listing).expect("repos JSON");
    let rows = report["repos"].as_array().expect("repos");

    // Then: exactly one row's upstream name is the claim's repo, and it is
    // the claimed fork's, with its checkout; the registry key matches none.
    let matching: Vec<&Value> = rows
        .iter()
        .filter(|row| row["upstream_name"] == claimed.as_str())
        .collect();
    assert_eq!(matching.len(), 1, "claim repo {claimed}: {report}");
    let row = matching[0];
    assert_eq!(row["name"], "demo", "{row}");
    assert_eq!(row["upstream_name"], KEPT_UNDER, "{row}");
    assert_eq!(
        row["path"].as_str().map(Path::new),
        Some(lab.work.canonicalize().expect("canonical work").as_path()),
        "{row}"
    );
    assert!(
        rows.iter().all(|row| row["name"] != claimed.as_str()),
        "{report}"
    );
    // And: a fork whose upstream is a filesystem path is kept under its key.
    let local = rows
        .iter()
        .find(|row| row["name"] == "local")
        .unwrap_or_else(|| panic!("no local row: {report}"));
    assert_eq!(local["upstream_name"], "local", "{local}");
}

/// The fork's status report, from a status run as the terminal user.
fn status_of_demo(lab: &Lab, home: &Path) -> Output {
    knives_as(
        lab,
        home,
        &lab.work,
        None,
        &["--json", "status", "demo", "--no-github", "--no-landed"],
    )
}

/// Before `knives ledger migrate`, status and the Claude Code hook both refuse
/// and name the command; after it, both open and show `agent-one`'s claim on
/// `feat/gamma`.
fn refused_until_migrated(lab: &Lab, home: &Path) {
    // When: status runs, and a Claude Code session starts in the fork.
    let refused = status_of_demo(lab, home);
    let notice = session_start_notice(home, &lab.work);

    // Then: status exits incomplete naming the command verbatim, and the
    // agent is told so in its context rather than shown an empty roster.
    let errors = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(3), "stderr: {errors}");
    assert!(
        errors.contains("Run `knives ledger migrate`"),
        "stderr: {errors}"
    );
    assert!(
        notice.contains("Run `knives ledger migrate`")
            && notice.contains("do not assume a branch is free"),
        "notice: {notice}"
    );
    assert!(!notice.contains("No branch is claimed"), "notice: {notice}");

    // When: the migration runs, and the same two run again.
    succeeded(&knives_as(
        lab,
        home,
        &lab.work,
        Some("ses_migrate"),
        &["--json", "ledger", "migrate"],
    ));
    let opened = succeeded(&status_of_demo(lab, home)).stdout.clone();
    let notice = session_start_notice(home, &lab.work);

    // Then: status opens with the claim on the branch's row, and the notice
    // names it.
    let report: Value = serde_json::from_slice(&opened).expect("status JSON");
    let row = report["branches"]
        .as_array()
        .expect("branches")
        .iter()
        .find(|row| row["name"] == "feat/gamma")
        .unwrap_or_else(|| panic!("no feat/gamma row: {report}"));
    assert_eq!(row["claim"]["id"], "agent-one", "row: {row}");
    assert!(
        notice.contains("feat/gamma (agent-one, harness-session, claimed "),
        "notice: {notice}"
    );
}

#[test]
fn a_claim_still_kept_under_the_registry_key_is_refused_until_migrated() {
    // Given: a claimed branch whose claim the state file keeps under the
    // fork's registry key, as an older knives wrote it.
    let (lab, home) = forge_fork();
    claim_gamma(&lab, home.path());
    let path = home.path().join("state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read state"))
            .expect("state JSON");
    let claims = state["claims"].as_object_mut().expect("claims");
    let mut claim = claims
        .remove(&format!("{KEPT_UNDER}/feat/gamma"))
        .expect("the claim under the upstream name");
    claim["repo"] = json!("demo");
    claims.insert("demo/feat/gamma".to_owned(), claim);
    std::fs::write(&path, state.to_string()).expect("write state");

    refused_until_migrated(&lab, home.path());
}

#[test]
fn a_ledger_still_kept_under_the_registry_key_is_refused_until_migrated() {
    // Given: a claimed branch, and a ledger entry an older knives filed in
    // the directory named for the fork's registry key.
    let (lab, home) = forge_fork();
    claim_gamma(&lab, home.path());
    knives::ledger::Ledger::at(home.path().join("ledger").join("demo"))
        .append(&knives::ledger::Entry {
            ts: "2026-01-01T00:00:00Z".to_owned(),
            owner: "ses_older".to_owned(),
            email: None,
            subject: None,
            kind: knives::ledger::Kind::Note,
            disposition: None,
            statement: None,
            text: "written by an older knives".to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: Vec::new(),
        })
        .expect("append a note");

    refused_until_migrated(&lab, home.path());
    assert!(
        !home.path().join("ledger/demo").exists(),
        "the registry key's directory is gone after migrating"
    );
}

/// A note an older knives filed under `demo`, its registry key.
fn note_under_the_registry_key(home: &Path) {
    knives::ledger::Ledger::at(home.join("ledger").join("demo"))
        .append(&knives::ledger::Entry {
            ts: "2026-01-01T00:00:00Z".to_owned(),
            owner: "ses_older".to_owned(),
            email: None,
            subject: None,
            kind: knives::ledger::Kind::Note,
            disposition: None,
            statement: None,
            text: "written by an older knives".to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: Vec::new(),
        })
        .expect("append a note");
}

#[test]
fn notch_and_the_release_plan_refuse_a_ledger_still_under_the_registry_key() {
    // Given: a ledger entry an older knives filed under the fork's registry
    // key, and nothing yet under its upstream name.
    let (lab, home) = forge_fork();
    note_under_the_registry_key(home.path());
    let readers: [&[&str]; 3] = [
        &["--text", "notch", "--repo", "demo"],
        &["--text", "notch", "--repo", "demo", "--pr", "1"],
        &["--text", "release", "--repo", "demo"],
    ];

    // When: notch reads the fork's ledger, and the release plan is asked for,
    // as an agent session would ask (no store read on the way).
    for args in readers {
        let refused = knives_as(&lab, home.path(), &lab.work, Some("agent-one"), args);

        // Then: each exits incomplete naming the command, rather than reading
        // an empty ledger as "no notches yet" or a plan with no recorded cut.
        let errors = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(
            refused.status.code(),
            Some(3),
            "{args:?}: stdout: {}\nstderr: {errors}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert!(
            errors.contains("Run `knives ledger migrate`") && errors.contains("ledger/demo"),
            "{args:?}: {errors}"
        );
        assert!(
            !String::from_utf8_lossy(&refused.stdout).contains("no notches yet"),
            "{args:?}"
        );
    }

    // When: the migration runs.
    succeeded(&knives_as(
        &lab,
        home.path(),
        &lab.work,
        Some("ses_migrate"),
        &["--json", "ledger", "migrate"],
    ));

    // Then: notch reads the entry under the fork's upstream name, and the
    // release plan answers.
    let read = succeeded(&knives_as(
        &lab,
        home.path(),
        &lab.work,
        Some("agent-one"),
        &["--text", "notch", "--repo", "demo"],
    ))
    .stdout
    .clone();
    assert!(
        String::from_utf8_lossy(&read).contains("written by an older knives"),
        "{}",
        String::from_utf8_lossy(&read)
    );
    let plan = knives_as(
        &lab,
        home.path(),
        &lab.work,
        Some("agent-one"),
        &["--text", "release", "--repo", "demo"],
    );
    assert!(
        !String::from_utf8_lossy(&plan.stderr).contains("knives ledger migrate"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
}
