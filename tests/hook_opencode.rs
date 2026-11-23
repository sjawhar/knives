#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "fixture setup failures and JSON shape mismatches are test failures"
)]

#[path = "common/lab.rs"]
mod lab;

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lab::git_repository;
use serde_json::{Value, json};

const SESSION_ID: &str = "opencode-hook-test-session";

fn run_hook_input(home: &Path, input: &str) -> (bool, String, String) {
    run_hook_input_with_owner(home, input, None)
}

fn run_hook_input_with_owner(
    home: &Path,
    input: &str,
    owner: Option<&str>,
) -> (bool, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command
        .args(["hook", "opencode"])
        .env("KNIVES_CONFIG_HOME", home)
        .env("HOME", home)
        .env_remove("KNIVES_OWNER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(owner) = owner {
        command.env("KNIVES_OWNER", owner);
    }
    let mut child = command.spawn().expect("spawn hook");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(input.as_bytes())
        .expect("write hook input");
    let output = child.wait_with_output().expect("wait for hook");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("hook output is UTF-8"),
        String::from_utf8(output.stderr).expect("hook errors are UTF-8"),
    )
}

fn run_hook(home: &Path, event: &Value) -> Value {
    run_hook_with_owner(home, event, None)
}

fn run_hook_with_owner(home: &Path, event: &Value, owner: Option<&str>) -> Value {
    let (success, output, errors) = run_hook_input_with_owner(home, &event.to_string(), owner);
    assert!(success, "a hook must never fail the session: {errors}");
    serde_json::from_str(&output).expect("hook output JSON")
}

fn addition(output: &Value) -> &str {
    output["addition"].as_str().expect("tool addition")
}

fn notice_attribute<'a>(addition: &'a str, name: &str) -> &'a str {
    let tag = addition
        .lines()
        .find(|line| line.starts_with("<knives-notice-"))
        .expect("notice opening tag");
    let prefix = format!("{name}=\"");
    tag.split_once(&prefix)
        .and_then(|(_, value)| value.split_once('"').map(|(value, _)| value))
        .expect("notice attribute")
}

fn notice_nonce(addition: &str) -> &str {
    let tag = addition
        .lines()
        .find(|line| line.starts_with("<knives-notice-"))
        .expect("notice opening tag");
    tag.strip_prefix("<knives-notice-")
        .and_then(|rest| rest.split_once(' ').map(|(nonce, _)| nonce))
        .expect("notice nonce")
}

fn claim(branch: &str) -> Value {
    json!({
        "repo": "maintainer/beta",
        "branch": branch,
        "owner": "agent-one",
        "why": "porting",
        "started": "2026-01-01T00:00:00Z",
        "files": []
    })
}

struct Repositories {
    home: tempfile::TempDir,
    /// Managed AND trusted: `origin` sits under a trusted owner.
    beta: PathBuf,
    /// Trusted only, through `[trust] repos`.
    trusted: PathBuf,
}

impl Repositories {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("config home");
        let beta = home.path().join("beta");
        let trusted = home.path().join("trusted");
        git_repository(
            &beta,
            &[
                ("upstream", "https://forge.invalid/maintainer/beta"),
                ("origin", "https://forge.invalid/ours/beta"),
            ],
        );
        git_repository(
            &trusted,
            &[("origin", "https://forge.invalid/company/trusted.git")],
        );
        for (root, instructions) in [
            (&beta, "beta instructions"),
            (&trusted, "trusted instructions"),
        ] {
            std::fs::write(root.join("AGENTS.md"), instructions).expect("write instructions");
            std::fs::write(root.join("file.txt"), "content").expect("write file");
        }
        let config = "[repos.beta]\nupstream = \"https://forge.invalid/maintainer/beta\"\n\
                      origin = \"https://forge.invalid/ours/beta\"\n\n\
                      [trust]\nowners = [\"ours\"]\nrepos = [\"company/trusted\"]\n";
        std::fs::write(home.path().join("repos.toml"), config).expect("write registry");
        let state = json!({"claims": {"maintainer/beta/feat/claimed": {
            "repo": "maintainer/beta", "branch": "feat/claimed", "owner": "agent-one",
            "why": "porting", "started": "2026-01-01T00:00:00Z", "files": []
        }}});
        std::fs::write(home.path().join("state.json"), state.to_string()).expect("write state");
        Self {
            home,
            beta,
            trusted,
        }
    }

    /// Turn a git-only fixture into a colocated jj checkout, so `seen` can key
    /// on its `.jj` and jj can still read its remotes.
    fn colocate(root: &Path) {
        lab::jj(root, ["git", "init", "--colocate"]);
    }

    fn write_state(&self, state: &Value) {
        std::fs::write(self.home.path().join("state.json"), state.to_string())
            .expect("write state");
    }
}

fn tool(path: &Path, parts: Option<Value>) -> Value {
    let mut event = json!({
        "event": "tool.execute.after", "session_id": SESSION_ID,
        "tool": "read", "args": {"filePath": path}
    });
    if let Some(parts) = parts {
        event["parts"] = parts;
    }
    event
}

#[test]
fn tool_after_emits_notice_and_guidance_once_with_one_shared_budget() {
    // Given: a managed repository with instructions and a claim.
    let repos = Repositories::new();
    let event = tool(&repos.beta.join("file.txt"), None);

    // When: it is read, then read again with different part options.
    let first = run_hook(repos.home.path(), &event);
    let second = run_hook(
        repos.home.path(),
        &tool(&repos.beta.join("file.txt"), Some(json!({"notice": false}))),
    );

    // Then: the first addition has both envelopes and spends the entire budget.
    assert!(addition(&first).contains("<knives-notice-"), "was: {first}");
    assert!(
        addition(&first).contains("<knives-guidance-"),
        "was: {first}"
    );
    assert_eq!(addition(&second), "");
}

#[test]
fn a_notice_only_event_does_not_spend_the_guidance_budget() {
    // Given: a managed repository that has both an outstanding notice and guidance.
    let repos = Repositories::new();

    // When: the first relevant event requests only notice, then guidance is requested.
    let notice_only = run_hook(
        repos.home.path(),
        &tool(
            &repos.beta.join("file.txt"),
            Some(json!({"notice": true, "guidance": false})),
        ),
    );
    let guidance = run_hook(
        repos.home.path(),
        &tool(
            &repos.beta.join("file.txt"),
            Some(json!({"notice": false, "guidance": true})),
        ),
    );

    // Then: the notice did not mark guidance as rendered.
    assert!(
        addition(&notice_only).contains("<knives-notice-"),
        "was: {notice_only}"
    );
    assert!(
        addition(&guidance).contains("<knives-guidance-"),
        "was: {guidance}"
    );
}

#[test]
fn the_same_roster_is_noticed_once_per_session() {
    // Removing content-aware notice tracking would re-inject on the second
    // identical event, creating repetitive hook output.
    let repos = Repositories::new();
    let event = tool(&repos.beta.join("file.txt"), None);

    let first = run_hook(repos.home.path(), &event);
    let second = run_hook(repos.home.path(), &event);

    assert_eq!(notice_attribute(addition(&first), "digest").len(), 16);
    assert_eq!(addition(&second), "");
}

#[test]
fn a_roster_change_re_emits_the_notice() {
    // A boolean "noticed" flag would suppress the second notice even though
    // the roster changed and the user needs the new branch in the response.
    let repos = Repositories::new();
    let event = tool(&repos.beta.join("file.txt"), None);

    let first = run_hook(repos.home.path(), &event);
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed"),
        "maintainer/beta/feat/new": claim("feat/new")
    }}));
    let second = run_hook(repos.home.path(), &event);

    assert!(
        addition(&second).contains("<knives-notice-"),
        "was: {second}"
    );
    assert!(addition(&second).contains("feat/new"), "was: {second}");
    assert_ne!(
        notice_attribute(addition(&first), "digest"),
        notice_attribute(addition(&second), "digest")
    );
}

#[test]
fn the_notice_tag_carries_a_stable_digest_and_a_fresh_nonce() {
    // Digesting the nonce would make equal rosters look different, while
    // reusing it would weaken the notice envelope's anti-injection boundary.
    let repos = Repositories::new();
    let mut first_event = tool(&repos.beta.join("file.txt"), None);
    first_event["session_id"] = json!("first-session");
    let mut second_event = tool(&repos.beta.join("file.txt"), None);
    second_event["session_id"] = json!("second-session");

    let first = run_hook(repos.home.path(), &first_event);
    let second = run_hook(repos.home.path(), &second_event);

    assert_eq!(
        notice_attribute(addition(&first), "digest"),
        notice_attribute(addition(&second), "digest")
    );
    assert_ne!(
        notice_nonce(addition(&first)),
        notice_nonce(addition(&second))
    );
}

impl Repositories {
    /// Another checkout of beta, in a directory named `name`: a workspace of
    /// the fork, named for its branch as `knives start` names one.
    fn beta_workspace(&self, name: &str) -> PathBuf {
        let workspace = self.home.path().join(name);
        git_repository(
            &workspace,
            &[
                ("upstream", "https://forge.invalid/maintainer/beta"),
                ("origin", "https://forge.invalid/ours/beta"),
            ],
        );
        std::fs::write(workspace.join("file.txt"), "content").expect("write file");
        workspace
    }
}

/// A claim on beta whose reason runs past one row.
fn claim_with_reason(branch: &str, why: &str) -> Value {
    let mut claim = claim(branch);
    claim["why"] = json!(why);
    claim
}

const LONG_REASON: &str = "porting the walker to the new layout, then the plant tests, then \
                           the review fixes, then the release notes for the change";

#[test]
fn the_same_roster_at_another_workspace_is_one_line() {
    // Given: a session that had beta's roster through one checkout.
    let repos = Repositories::new();
    let first = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    let digest = notice_attribute(addition(&first), "digest").to_owned();

    // When: a file in another workspace of beta is read, the roster unchanged.
    let other = repos.beta_workspace("feat-other");
    let second = run_hook(repos.home.path(), &tool(&other.join("file.txt"), None));
    let third = run_hook(repos.home.path(), &tool(&other.join("file.txt"), None));

    // Then: one line saying so, naming no claim; then nothing. The checkout
    // has no commit, so no branch is read checked out there, and nothing is
    // said of claims on it.
    let short = addition(&second);
    assert_eq!(notice_attribute(short, "digest"), digest);
    assert!(
        short.contains(&format!(
            "the claims on this fork are unchanged since the notice with digest {digest}.\n\
             </knives-notice-"
        )),
        "was: {short}"
    );
    assert!(!short.contains("feat/claimed"), "was: {short}");
    assert!(!short.contains("knives start"), "was: {short}");
    assert_eq!(addition(&third), "");
}

#[test]
fn a_changed_claim_is_told_alone() {
    // Given: a session that had a roster of two claims.
    let repos = Repositories::new();
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed"),
        "maintainer/beta/feat/other": claim("feat/other")
    }}));
    let event = tool(&repos.beta.join("file.txt"), None);
    let first = run_hook(repos.home.path(), &event);
    assert!(addition(&first).contains("feat/other"), "was: {first}");

    // When: one claim's reason changes, and a file is read again.
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed"),
        "maintainer/beta/feat/other": claim_with_reason("feat/other", "review fixes")
    }}));
    let second = run_hook(repos.home.path(), &event);

    // Then: that claim alone, as changed, and none of the unchanged one.
    let changes = addition(&second);
    assert!(
        changes.contains(&format!(
            "changed since the notice with digest {}",
            notice_attribute(addition(&first), "digest")
        )),
        "was: {changes}"
    );
    assert!(
        changes.contains("- claim changed: feat/other (agent-one,"),
        "was: {changes}"
    );
    assert!(changes.contains("): review fixes\n"), "was: {changes}");
    assert!(!changes.contains("feat/claimed"), "was: {changes}");
    assert!(!changes.contains("knives start"), "was: {changes}");
}

#[test]
fn the_claim_on_a_touched_workspaces_branch_is_given_whole() {
    // Given: a claim whose reason runs past one row, and a session that had
    // the roster through the main checkout, the reason cut there.
    let repos = Repositories::new();
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", LONG_REASON)
    }}));
    let first = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    assert!(
        !addition(&first).contains(LONG_REASON),
        "the roster cuts the reason: {first}"
    );
    assert!(addition(&first).contains("feat/claimed"), "was: {first}");

    // When: a file in that branch's workspace is read, then read again.
    let workspace = repos.beta_workspace("feat-claimed");
    let second = run_hook(repos.home.path(), &tool(&workspace.join("file.txt"), None));
    let third = run_hook(repos.home.path(), &tool(&workspace.join("file.txt"), None));

    // Then: who holds the branch, with the whole reason, once.
    let whole = addition(&second);
    assert!(
        whole.contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {whole}"
    );
    assert!(
        whole.contains(&format!("): {LONG_REASON}\n")),
        "was: {whole}"
    );
    assert_eq!(addition(&third), "");
}

/// Commits in `workspace` and checks out `branch` there, as a checkout made
/// by hand has it.
fn check_out(workspace: &Path, branch: &str) {
    for args in [
        &["add", "file.txt"][..],
        &[
            "-c",
            "user.name=lab",
            "-c",
            "user.email=lab@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "work",
        ][..],
        &["checkout", "--quiet", "-b", branch][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(workspace)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }
}

#[test]
fn a_checkout_of_a_claimed_branch_is_told_the_claim_whatever_its_name() {
    // Given: a session that had beta's roster through the main checkout, and
    // a checkout named `my-fix` with the claimed branch checked out.
    let repos = Repositories::new();
    let _ = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    let workspace = repos.beta_workspace("my-fix");
    check_out(&workspace, "feat/claimed");

    // When: a file there is read.
    let output = run_hook(repos.home.path(), &tool(&workspace.join("file.txt"), None));

    // Then: the claim on the branch checked out is told, never that no
    // claimed branch has its workspace here (B1).
    let text = addition(&output);
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {text}"
    );
    assert!(!text.contains("No claim"), "was: {text}");
}

#[test]
fn a_session_that_lost_its_context_is_told_its_roster_again() {
    // Given: a session that had beta's guidance and roster in one block.
    let repos = Repositories::new();
    let file = repos.beta.join("file.txt");
    let first = run_hook(repos.home.path(), &tool_in_context(&file, "first", &[]));
    assert!(addition(&first).contains("feat/claimed"), "was: {first}");
    let held = run_hook(
        repos.home.path(),
        &tool_in_context(&file, "held", &[addition(&first)]),
    );
    assert_eq!(addition(&held), "");

    // When: the context no longer holds the block, as after `/clear`, a shake
    // or a compaction of the harness.
    let lost = run_hook(repos.home.path(), &tool_in_context(&file, "lost", &[]));

    // Then: the guidance and the roster both come again.
    let text = addition(&lost);
    assert!(text.contains("beta instructions"), "the control: {text}");
    assert!(text.contains("Branches claimed here (1)"), "was: {text}");
    assert!(text.contains("feat/claimed"), "was: {text}");
}

#[test]
fn after_a_lost_context_a_changed_claim_comes_with_the_whole_roster() {
    // Given: a session that had the roster of two claims through feat/claimed's
    // workspace, and then lost the block that told it.
    let repos = Repositories::new();
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed"),
        "maintainer/beta/feat/other": claim("feat/other")
    }}));
    let file = repos.beta_workspace("feat-claimed").join("file.txt");
    let first = run_hook(repos.home.path(), &tool_in_context(&file, "first", &[]));
    assert!(addition(&first).contains("feat/other"), "was: {first}");

    // When: the other claim changes, and a file is read with the context lost.
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed"),
        "maintainer/beta/feat/other": claim_with_reason("feat/other", "review fixes")
    }}));
    let lost = run_hook(repos.home.path(), &tool_in_context(&file, "lost", &[]));

    // Then: the touched branch's claim and the changed one, not the delta.
    let text = addition(&lost);
    assert!(text.contains("review fixes"), "the control: {text}");
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {text}"
    );
    assert!(!text.contains("only the changes follow"), "was: {text}");
}

/// Which omp extension sends a session's tool events.
#[derive(Debug, Clone, Copy)]
enum Extension {
    /// 5.1.0 to 6.1.4's, which reports the guidance blocks alone
    /// ([`tool_in_old_context`]).
    Old,
    /// This tree's, which reports the claims notices too ([`tool_in_context`]).
    New,
}

/// An omp session that reads a file once a turn, and whose context holds
/// every tool result it got until omp's shake takes one out.
struct OmpSession<'a> {
    home: &'a Path,
    extension: Extension,
    results: Vec<String>,
}

impl<'a> OmpSession<'a> {
    const fn new(home: &'a Path, extension: Extension) -> Self {
        Self {
            home,
            extension,
            results: Vec::new(),
        }
    }

    /// The next turn's read of `file`: what the hook added to its result.
    fn read(&mut self, file: &Path) -> String {
        let turn = format!("turn-{}", self.results.len() + 1);
        let held = self.results.iter().map(String::as_str).collect::<Vec<_>>();
        let event = match self.extension {
            Extension::Old => tool_in_old_context(file, &turn, &held),
            Extension::New => tool_in_context(file, &turn, &held),
        };
        let added = addition(&run_hook(self.home, &event)).to_owned();
        self.results.push(added.clone());
        added
    }

    /// omp's shake: the result of turn `turn`, counted from 1, leaves the
    /// context.
    fn shake(&mut self, turn: usize) {
        self.results[turn - 1] = "[shaken]".to_owned();
    }
}

/// beta's claims: feat/claimed, and feat/other with the reason `why`.
fn two_claims(why: &str) -> Value {
    json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed"),
        "maintainer/beta/feat/other": claim_with_reason("feat/other", why)
    }})
}

/// What a session whose omp extension is `extension` was given on each of
/// six turns that read beta's file, feat/other's reason changed after the
/// first when `change`.
fn six_turns(extension: Extension, change: bool) -> Vec<String> {
    let repos = Repositories::new();
    repos.write_state(&two_claims("porting"));
    let file = repos.beta.join("file.txt");
    let mut session = OmpSession::new(repos.home.path(), extension);
    let mut turns = vec![session.read(&file)];
    if change {
        repos.write_state(&two_claims("review fixes"));
    }
    for _ in 2..=6 {
        turns.push(session.read(&file));
    }
    turns
}

#[test]
fn an_old_extension_is_told_a_change_once_and_not_the_roster_on_every_turn() {
    // Given: a session whose omp extension predates the report of claims
    // notices (5.1.0 to 6.1.4), reading beta on six turns, a claim changed
    // after the first.
    let turns = six_turns(Extension::Old, true);

    // Then: the roster, the change, and nothing more. The change's notice has
    // no guidance block beside it, so that extension never reports it; a
    // binary that took its absence as the notice lost told the whole roster
    // again on every later turn.
    assert!(turns[0].contains("Branches claimed here (2)"), "{turns:#?}");
    assert!(
        turns[1].contains("- claim changed: feat/other (agent-one,"),
        "{turns:#?}"
    );
    assert_eq!(turns[2..], ["", "", "", ""], "{turns:#?}");
}

#[test]
fn the_new_extension_is_told_a_change_once() {
    // Given: the same six turns, from an extension that reports claims notices.
    let turns = six_turns(Extension::New, true);

    // Then: the roster, the change, and nothing more.
    assert!(turns[0].contains("Branches claimed here (2)"), "{turns:#?}");
    assert!(
        turns[1].contains("- claim changed: feat/other (agent-one,"),
        "{turns:#?}"
    );
    assert_eq!(turns[2..], ["", "", "", ""], "{turns:#?}");
}

#[test]
fn an_old_extension_with_no_change_is_told_the_roster_once() {
    // Given: six turns from the old extension, and no claim changed.
    let turns = six_turns(Extension::Old, false);

    // Then: the roster on the first turn, and nothing after it.
    assert!(turns[0].contains("Branches claimed here (2)"), "{turns:#?}");
    assert_eq!(turns[1..], ["", "", "", "", ""], "{turns:#?}");
}

#[test]
fn the_new_extension_is_told_the_roster_again_once_a_shake_takes_the_change() {
    // Given: a session whose extension reports claims notices, told beta's
    // roster and then a change.
    let repos = Repositories::new();
    repos.write_state(&two_claims("porting"));
    let file = repos.beta.join("file.txt");
    let mut session = OmpSession::new(repos.home.path(), Extension::New);
    let _ = session.read(&file);
    repos.write_state(&two_claims("review fixes"));
    let change = session.read(&file);
    assert!(
        change.contains("- claim changed: feat/other (agent-one,"),
        "the control: {change}"
    );

    // When: omp's shake takes the change's result out of the context.
    session.shake(2);
    let lost = session.read(&file);
    let after = session.read(&file);

    // Then: the whole roster, with the change, once.
    assert!(
        lost.contains("Branches claimed here (2)") && lost.contains("review fixes"),
        "{lost}"
    );
    assert_eq!(after, "");
}

#[test]
fn an_old_extension_that_lost_its_guidance_is_given_it_again_and_keeps_its_roster() {
    // Given: a session whose extension predates notice reports, told beta's
    // guidance and roster in one result.
    let repos = Repositories::new();
    let file = repos.beta.join("file.txt");
    let mut session = OmpSession::new(repos.home.path(), Extension::Old);
    let first = session.read(&file);
    assert!(
        first.contains("beta instructions") && first.contains("<knives-notice-"),
        "the control: {first}"
    );

    // When: omp's shake takes that result out of the context.
    session.shake(1);
    let lost = session.read(&file);

    // Then: the guidance again, which that extension reports. Its report says
    // nothing of claims notices, so the roster the binary told stands, as it
    // did before 6.1.5.
    assert!(lost.contains("beta instructions"), "{lost}");
    assert!(!lost.contains("<knives-notice-"), "{lost}");
}

#[test]
fn a_session_resumed_with_the_new_extension_is_told_a_roster_the_old_one_left_unchecked() {
    // Given: a session whose old extension was told beta's roster, then a
    // change whose notice it never reports, then guidance in another
    // repository, whose call recorded the change's notice as gone.
    let repos = Repositories::new();
    repos.write_state(&two_claims("porting"));
    let file = repos.beta.join("file.txt");
    let mut session = OmpSession::new(repos.home.path(), Extension::Old);
    let _ = session.read(&file);
    repos.write_state(&two_claims("review fixes"));
    let change = session.read(&file);
    let elsewhere = session.read(&repos.trusted.join("file.txt"));
    assert!(
        change.contains("- claim changed: feat/other (agent-one,"),
        "the control: {change}"
    );
    assert!(
        elsewhere.contains("trusted instructions") && !elsewhere.contains("<knives-notice-"),
        "{elsewhere}"
    );

    // When: the session is resumed in an omp that loads the new extension.
    session.extension = Extension::New;
    let resumed = session.read(&file);
    let after = session.read(&file);

    // Then: the whole roster, once: the record of the change's notice went
    // with the old report, so nothing says whether the context still holds it.
    assert!(
        resumed.contains("Branches claimed here (2)") && resumed.contains("review fixes"),
        "{resumed}"
    );
    assert_eq!(after, "");
}

#[test]
fn a_session_resumed_with_the_new_extension_checks_a_change_the_old_one_never_reported() {
    // Given: a session whose old extension was told beta's roster, then a
    // change whose notice it never reports, and calls that gave nothing,
    // so nothing was recorded as gone.
    let repos = Repositories::new();
    repos.write_state(&two_claims("porting"));
    let file = repos.beta.join("file.txt");
    let mut session = OmpSession::new(repos.home.path(), Extension::Old);
    let _ = session.read(&file);
    repos.write_state(&two_claims("review fixes"));
    let change = session.read(&file);
    let quiet = session.read(&file);
    assert!(
        change.contains("- claim changed: feat/other (agent-one,"),
        "the control: {change}"
    );
    assert_eq!(quiet, "");

    // When: it is resumed with the new extension, and omp's shake later takes
    // the change's result out of the context.
    session.extension = Extension::New;
    let resumed = session.read(&file);
    session.shake(2);
    let lost = session.read(&file);

    // Then: nothing while the context holds the change's notice; the whole
    // roster, with the change, once it is gone.
    assert_eq!(resumed, "");
    assert!(
        lost.contains("Branches claimed here (2)") && lost.contains("review fixes"),
        "{lost}"
    );
}

#[test]
fn a_session_resumed_with_the_new_extension_checks_the_blocks_the_old_one_reported() {
    // Given: a session whose old extension was told beta's guidance and
    // roster in one result.
    let repos = Repositories::new();
    let file = repos.beta.join("file.txt");
    let mut session = OmpSession::new(repos.home.path(), Extension::Old);
    let _ = session.read(&file);

    // When: it is resumed with the new extension, and omp's shake later takes
    // that result out of the context.
    session.extension = Extension::New;
    let resumed = session.read(&file);
    session.shake(1);
    let lost = session.read(&file);

    // Then: nothing while the context holds the result; the guidance and the
    // roster once it is gone.
    assert_eq!(resumed, "");
    assert!(
        lost.contains("beta instructions") && lost.contains("Branches claimed here (1)"),
        "{lost}"
    );
}

#[test]
fn a_reason_changed_back_is_told_where_it_was_last_shown_otherwise() {
    // Given: a session shown feat/claimed's claim whole in its workspace,
    // then shown it changed elsewhere.
    let repos = Repositories::new();
    let here = tool(&repos.beta_workspace("feat-claimed").join("file.txt"), None);
    let elsewhere = tool(&repos.beta.join("file.txt"), None);
    let _ = run_hook(repos.home.path(), &here);
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", "review fixes")
    }}));
    let changed = run_hook(repos.home.path(), &elsewhere);
    assert!(
        addition(&changed).contains("review fixes"),
        "was: {changed}"
    );

    // When: the reason changes back, and the workspace is read again.
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed")
    }}));
    let reverted = run_hook(repos.home.path(), &here);

    // Then: the claim as it is now, since the last the session saw was the
    // other reason (older than #169: main also said nothing).
    let text = addition(&reverted);
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {text}"
    );
    assert!(text.contains("): porting\n"), "was: {text}");
}

#[test]
fn a_claim_taken_again_after_its_release_is_told() {
    // Given: a session shown feat/claimed's claim in its workspace, then its
    // release elsewhere.
    let repos = Repositories::new();
    let here = tool(&repos.beta_workspace("feat-claimed").join("file.txt"), None);
    let _ = run_hook(repos.home.path(), &here);
    repos.write_state(&json!({"claims": {}}));
    let released = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    assert!(
        addition(&released).contains("- released: feat/claimed"),
        "was: {released}"
    );

    // When: the same claim is taken again, and the workspace is read.
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed")
    }}));
    let restored = run_hook(repos.home.path(), &here);

    // Then: it is told, since the last the session saw was its release
    // (older than #169).
    let text = addition(&restored);
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {text}"
    );
}

#[test]
fn a_reason_changed_back_is_told_at_a_new_workspace_too() {
    // Given: as above, but the changed-back claim read at another workspace
    // of the branch, never touched before.
    let repos = Repositories::new();
    let first = tool(&repos.beta_workspace("feat-claimed").join("file.txt"), None);
    let _ = run_hook(repos.home.path(), &first);
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", "review fixes")
    }}));
    let _ = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim("feat/claimed")
    }}));

    // When: the other workspace is read.
    let second = repos.beta_workspace("copy/feat-claimed");
    let reverted = run_hook(repos.home.path(), &tool(&second.join("file.txt"), None));

    // Then: the claim as it is now.
    let text = addition(&reverted);
    assert!(text.contains("agent-one"), "was: {text}");
    assert!(text.contains("porting"), "was: {text}");
}

#[test]
fn a_new_holder_of_a_touched_branch_is_named_whole() {
    // Given: a session shown feat/claimed's claim in its workspace.
    let repos = Repositories::new();
    let here = tool(&repos.beta_workspace("feat-claimed").join("file.txt"), None);
    let _ = run_hook(repos.home.path(), &here);

    // When: another agent holds the branch now.
    let mut newer = claim_with_reason("feat/claimed", LONG_REASON);
    newer["owner"] = json!("agent-two");
    newer["started"] = json!("2026-01-02T00:00:00Z");
    repos.write_state(&json!({"claims": {"maintainer/beta/feat/claimed": newer}}));
    let output = run_hook(repos.home.path(), &here);

    // Then: the new holder, with the whole reason, and not the old one.
    let text = addition(&output);
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed (agent-two,"),
        "was: {text}"
    );
    assert!(text.contains(LONG_REASON), "was: {text}");
    assert!(!text.contains("agent-one"), "was: {text}");
}

/// The claims notice in `addition`, from its opening line to its closing tag.
fn notice_block(addition: &str) -> &str {
    let start = addition.find("\n\n<knives-notice-").expect("a notice");
    let close = start + addition[start..].find("</knives-notice-").expect("its end");
    let end = close + addition[close..].find('>').expect("the tag's end") + 1;
    &addition[start..end]
}

/// The session record of how the claim on feat/claimed was last shown.
fn feat_claimed_given(home: &Path) -> Value {
    let file = home
        .join("hook-sessions")
        .join(format!("opencode-{SESSION_ID}.json"));
    let state: Value =
        serde_json::from_str(&std::fs::read_to_string(&file).expect("the session's record"))
            .expect("session JSON");
    state["shown_claims"]["maintainer/beta\u{1f}feat/claimed"]["given"].clone()
}

#[test]
fn a_touched_claim_whose_reason_is_over_the_cap_is_cut_and_not_recorded_whole() {
    // Given: feat/claimed's claim with a reason of 10,000 characters, most of
    // them more than one byte.
    let repos = Repositories::new();
    let reason = "é—x".repeat(3_333) + "é";
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", &reason)
    }}));
    let here = tool(&repos.beta_workspace("feat-claimed").join("file.txt"), None);

    // When: its workspace is read.
    let first = run_hook(repos.home.path(), &here);

    // Then: the notice is within the cap, keeps the branch and the holder,
    // and says how much of the reason it shows; the session records the
    // claim as fitted, not whole.
    let notice = notice_block(addition(&first));
    assert!(
        notice.chars().count() <= 9_500,
        "{}",
        notice.chars().count()
    );
    assert!(
        notice.contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {notice}"
    );
    assert!(notice.contains("… (reason cut: "), "was: {notice}");
    assert!(notice.contains(" of 10000 characters)\n"), "was: {notice}");
    assert_eq!(feat_claimed_given(repos.home.path()), json!("fitted"));

    // And: unchanged, it is not told again; changed, it is.
    assert_eq!(addition(&run_hook(repos.home.path(), &here)), "");
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", &format!("{reason}!"))
    }}));
    let changed = run_hook(repos.home.path(), &here);
    assert!(
        addition(&changed).contains("This workspace's branch is claimed: feat/claimed (agent-one,"),
        "was: {changed}"
    );
}

/// Runs git in `dir` with a fixed identity.
fn git_in(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Lab",
            "-c",
            "user.email=lab@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_workspace_named_for_its_claim_with_head_where_main_is_is_told_its_claim() {
    // Given: the shape of a workspace `knives start` made for a new branch:
    // named for the claimed branch, HEAD detached at the base commit, which
    // the local `main` holds too.
    let repos = Repositories::new();
    let _ = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    let workspace = repos.beta_workspace("feat-claimed");
    git_in(&workspace, &["add", "file.txt"]);
    git_in(&workspace, &["commit", "-q", "-m", "trunk"]);
    git_in(&workspace, &["checkout", "-q", "--detach"]);

    // When: a file there is read.
    let output = run_hook(repos.home.path(), &tool(&workspace.join("file.txt"), None));

    // Then: its claim, never that no claim is on `main` (F1).
    let text = addition(&output);
    assert!(
        !text.contains("No claim is on the branch checked out here"),
        "was: {text}"
    );
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed"),
        "was: {text}"
    );
}

#[test]
fn a_fresh_jj_checkout_named_for_its_claim_is_told_its_claim() {
    // Given: the same with jj itself: a colocated jj checkout whose working
    // copy is a new change on `main`.
    let repos = Repositories::new();
    let _ = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));
    let workspace = repos.beta_workspace("feat-claimed");
    git_in(&workspace, &["add", "file.txt"]);
    git_in(&workspace, &["commit", "-q", "-m", "trunk"]);
    let init = Command::new("jj")
        .current_dir(&workspace)
        .args(["git", "init", "--colocate"])
        .output()
        .expect("run jj");
    assert!(
        init.status.success(),
        "jj git init: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // When: a file there is read.
    let output = run_hook(repos.home.path(), &tool(&workspace.join("file.txt"), None));

    // Then: its claim (F1).
    let text = addition(&output);
    assert!(
        !text.contains("No claim is on the branch checked out here"),
        "was: {text}"
    );
    assert!(
        text.contains("This workspace's branch is claimed: feat/claimed"),
        "was: {text}"
    );
}

/// feat/claimed's claim with a reason too long for the cap, at its
/// workspace: the session's repositories and the read of a file there.
fn a_fitted_claim() -> (Repositories, PathBuf, String) {
    let repos = Repositories::new();
    let reason = "é—x".repeat(3_333) + "é";
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", &reason)
    }}));
    let file = repos.beta_workspace("feat-claimed").join("file.txt");
    (repos, file, reason)
}

#[test]
fn a_fitted_claim_is_told_again_fitted_after_a_lost_context() {
    // Given: a session told feat/claimed's claim fitted to the cap.
    let (repos, file, _) = a_fitted_claim();
    let first = run_hook(repos.home.path(), &tool_in_context(&file, "turn-1", &[]));
    let first = addition(&first).to_owned();
    assert!(first.contains("… (reason cut: "), "{first}");
    assert_eq!(feat_claimed_given(repos.home.path()), json!("fitted"));

    // When: the context holds the notice, and then loses it.
    let held = run_hook(
        repos.home.path(),
        &tool_in_context(&file, "turn-2", &[first.as_str()]),
    );
    let lost = run_hook(repos.home.path(), &tool_in_context(&file, "turn-3", &[]));

    // Then: nothing while it is held; fitted again once lost, never as one
    // line (the review's plant that kept what was shown gave a line).
    assert_eq!(addition(&held), "", "the context still holds the notice");
    let lost = addition(&lost);
    assert!(
        lost.contains("This workspace's branch is claimed: feat/claimed (agent-one,")
            && lost.contains("… (reason cut: "),
        "{lost}"
    );
}

#[test]
fn a_fitted_claim_that_changed_is_fitted_again() {
    // Given: a session told feat/claimed's claim fitted to the cap.
    let (repos, file, reason) = a_fitted_claim();
    let here = tool(&file, None);
    let _ = run_hook(repos.home.path(), &here);

    // When: its reason changes.
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/claimed": claim_with_reason("feat/claimed", &format!("{reason}!"))
    }}));
    let changed = run_hook(repos.home.path(), &here);

    // Then: told again as fully as the cap allows, not as one line (the
    // review's plant that ignored a fitted claim's fingerprint gave a line).
    let text = addition(&changed);
    assert!(text.contains("… (reason cut: "), "{text}");
    assert_eq!(feat_claimed_given(repos.home.path()), json!("fitted"));
}

#[test]
fn a_registry_key_of_10000_characters_keeps_the_notice_within_its_cap() {
    // Given: beta registered under a key of 10,000 characters, which the
    // registry takes (the review's case rendered 10,376).
    let repos = Repositories::new();
    let key = "k".repeat(10_000);
    let registry = repos.home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        text.replace("[repos.beta]", &format!("[repos.{key}]")),
    )
    .expect("rewrite the registry");

    // When: a file in beta is read.
    let output = run_hook(repos.home.path(), &tool(&repos.beta.join("file.txt"), None));

    // Then: the whole notice within its cap, the key cut in the envelope.
    let notice = notice_block(addition(&output));
    assert!(
        notice.chars().count() <= 9_500,
        "{}",
        notice.chars().count()
    );
    assert!(
        notice.contains(&format!(" repo=\"{}...\" ", "k".repeat(100))),
        "{}",
        &notice[..300.min(notice.len())]
    );
    assert!(notice.contains("feat/claimed"), "{notice}");
}

#[test]
fn tool_after_in_a_managed_workspace_records_event_identity_and_cwd() {
    let repos = Repositories::new();
    Repositories::colocate(&repos.beta);
    let mut event = tool(&repos.beta.join("file.txt"), None);
    event["cwd"] = json!(repos.beta);

    let _ = run_hook(repos.home.path(), &event);

    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(repos.home.path().join("seen.json"))
            .expect("OpenCode hook records seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["owners"]["harness-session"][SESSION_ID]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
    assert!(
        seen["workspaces"]["maintainer/beta/beta"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
}

#[test]
fn tool_after_honors_disabled_notice_and_trusted_roots() {
    // Given: managed and trusted repositories with instructions.
    let repos = Repositories::new();

    // When: managed guidance disables its notice and trusted guidance explicitly permits all parts.
    let managed = run_hook(
        repos.home.path(),
        &tool(
            &repos.beta.join("file.txt"),
            Some(json!({"notice": false, "guidance": true})),
        ),
    );
    let trusted = run_hook(
        repos.home.path(),
        &tool(
            &repos.trusted.join("file.txt"),
            Some(json!({"notice": true, "guidance": true})),
        ),
    );

    // Then: guidance is emitted without a managed-fork notice in both cases.
    for output in [&managed, &trusted] {
        assert!(
            addition(output).contains("<knives-guidance-"),
            "was: {output}"
        );
        assert!(
            !addition(output).contains("<knives-notice-"),
            "was: {output}"
        );
    }
}

#[test]
fn tool_after_trust_roots_injects_guidance_without_managed_notice() {
    // Given: an unregistered checkout with AGENTS.md and a [trust].roots config entry.
    let home = tempfile::tempdir().expect("config home");
    let trust_root = home.path().join("unregistered-trust-root");
    std::fs::create_dir_all(trust_root.join(".git")).expect("create checkout");
    std::fs::write(trust_root.join("AGENTS.md"), "trust root instructions")
        .expect("write trust instructions");
    std::fs::write(trust_root.join("file.txt"), "content").expect("write file");
    std::fs::write(
        home.path().join("repos.toml"),
        format!("[trust]\nroots = [\"{}\"]\n", trust_root.display()),
    )
    .expect("write trust config");

    // When: a tool.execute.after event reads a file under the [trust].roots path.
    let output = run_hook(
        home.path(),
        &tool(
            &trust_root.join("file.txt"),
            Some(json!({"notice": true, "guidance": true})),
        ),
    );

    // Then: guidance is injected but the managed notice is never emitted.
    assert!(
        addition(&output).contains("<knives-guidance-"),
        "trust roots must inject guidance: {output}"
    );
    assert!(
        !addition(&output).contains("<knives-notice-"),
        "trust roots must not emit managed notice: {output}"
    );
}

/// A tool event from a harness that can see its session's system prompt.
fn tool_seeing(path: &Path, system: &[&str]) -> Value {
    let mut event = tool(path, None);
    event["system"] = json!(system);
    event
}

#[test]
fn guidance_the_system_prompt_already_carries_is_not_injected_again() {
    // Given: a session standing in beta, whose harness loaded beta's AGENTS.md
    // into the system prompt.
    let repos = Repositories::new();
    let system = [
        "You are an agent.",
        "<file path=\"beta/AGENTS.md\">\nbeta instructions\n</file>",
    ];

    // When: a file in beta is read.
    let output = run_hook(
        repos.home.path(),
        &tool_seeing(&repos.beta.join("file.txt"), &system),
    );

    // Then: the fork's live notice still arrives, the instructions do not arrive twice.
    assert!(
        addition(&output).contains("<knives-notice-"),
        "was: {output}"
    );
    assert!(
        !addition(&output).contains("<knives-guidance-"),
        "was: {output}"
    );
}

#[test]
fn guidance_for_a_repository_outside_the_system_prompt_still_arrives() {
    // Given: a session whose system prompt carries beta's instructions only.
    let repos = Repositories::new();

    // When: a file in another trusted repository is read.
    let output = run_hook(
        repos.home.path(),
        &tool_seeing(&repos.trusted.join("file.txt"), &["beta instructions"]),
    );

    // Then: that repository's instructions are injected.
    assert!(
        addition(&output).contains("<knives-guidance-"),
        "was: {output}"
    );
    assert!(
        addition(&output).contains("trusted instructions"),
        "was: {output}"
    );
}

#[test]
fn only_the_instruction_files_the_system_prompt_lacks_are_injected() {
    // Given: beta's root instructions are in the system prompt, a subdirectory's are not.
    let repos = Repositories::new();
    std::fs::create_dir_all(repos.beta.join("sub")).expect("create subdirectory");
    std::fs::write(repos.beta.join("sub/AGENTS.md"), "sub instructions")
        .expect("write nested instructions");
    std::fs::write(repos.beta.join("sub/file.txt"), "content").expect("write nested file");

    // When: a file under the subdirectory is read.
    let output = run_hook(
        repos.home.path(),
        &tool_seeing(&repos.beta.join("sub/file.txt"), &["beta instructions"]),
    );

    // Then: the nested instructions arrive without a second copy of the root's.
    assert!(
        addition(&output).contains("sub instructions"),
        "was: {output}"
    );
    assert!(
        !addition(&output).contains("beta instructions"),
        "was: {output}"
    );
}

#[test]
fn the_same_instructions_from_a_second_checkout_are_not_injected_twice() {
    // Given: two checkouts of one trusted repository carrying the same AGENTS.md.
    let repos = Repositories::new();
    let second = repos.home.path().join("trusted-second");
    git_repository(
        &second,
        &[("origin", "https://forge.invalid/company/trusted.git")],
    );
    std::fs::write(second.join("AGENTS.md"), "trusted instructions").expect("write instructions");
    std::fs::write(second.join("file.txt"), "content").expect("write file");

    // When: a file in each is read in one session.
    let first = run_hook(
        repos.home.path(),
        &tool(&repos.trusted.join("file.txt"), None),
    );
    let repeat = run_hook(repos.home.path(), &tool(&second.join("file.txt"), None));

    // Then: the instructions arrive once.
    assert!(
        addition(&first).contains("trusted instructions"),
        "was: {first}"
    );
    assert_eq!(addition(&repeat), "", "was: {repeat}");
}

#[test]
fn concurrent_tool_calls_inject_the_guidance_once() {
    // Given: one session issuing several tool calls in one turn, which the
    // harness runs in parallel.
    let repos = Repositories::new();
    let event = tool(&repos.trusted.join("file.txt"), None).to_string();

    // When: their hooks run at the same time.
    #[expect(
        clippy::needless_collect,
        reason = "every hook must be running before the first is awaited"
    )]
    let children: Vec<_> = (0..8)
        .map(|_| {
            let mut child = Command::new(env!("CARGO_BIN_EXE_knives"))
                .args(["hook", "opencode"])
                .env("KNIVES_CONFIG_HOME", repos.home.path())
                .env("HOME", repos.home.path())
                .env_remove("KNIVES_OWNER")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn hook");
            child
                .stdin
                .take()
                .expect("hook stdin")
                .write_all(event.as_bytes())
                .expect("write hook input");
            child
        })
        .collect();
    let additions: Vec<String> = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().expect("wait for hook");
            let value: Value = serde_json::from_slice(&output.stdout).expect("hook output JSON");
            addition(&value).to_owned()
        })
        .collect();

    // Then: exactly one of them carries the guidance.
    let guided = additions
        .iter()
        .filter(|addition| addition.contains("<knives-guidance-"))
        .count();
    assert_eq!(guided, 1, "was: {additions:?}");
}

#[test]
fn a_call_that_finds_the_session_record_held_leaves_the_guidance_for_a_later_call() {
    // Given: another hook call of the same session holding its record's lock.
    let repos = Repositories::new();
    let sessions = repos.home.path().join("hook-sessions");
    std::fs::create_dir_all(&sessions).expect("sessions directory");
    let lock = File::create(sessions.join(format!("opencode-{SESSION_ID}.lock")))
        .expect("session lock file");
    lock.lock().expect("hold the session lock");
    let event = tool(&repos.trusted.join("file.txt"), None);

    // When: a file in a trusted repository is read while it is held, then after.
    let held = run_hook(repos.home.path(), &event);
    lock.unlock().expect("release the session lock");
    let released = run_hook(repos.home.path(), &event);

    // Then: the waiting call renders nothing rather than a second copy, and
    // the guidance is still due for the next call.
    assert_eq!(addition(&held), "", "was: {held}");
    assert!(
        addition(&released).contains("trusted instructions"),
        "was: {released}"
    );
}

/// A tool event as the omp extension sends it: the turn that made the call,
/// and the knives blocks in the tool results the model's context still holds
/// (`held`): every block's envelope nonce in `guidance`, and the claims
/// notices' in `notices` too.
fn tool_in_context(path: &Path, turn: &str, held: &[&str]) -> Value {
    let notices = held
        .iter()
        .flat_map(|text| block_nonces(text, "notice"))
        .collect::<BTreeSet<_>>();
    let mut blocks = held
        .iter()
        .flat_map(|text| block_nonces(text, "guidance"))
        .collect::<BTreeSet<_>>();
    blocks.extend(&notices);
    let mut event = tool(path, None);
    event["context"] = json!({"turn": turn, "guidance": blocks, "notices": notices});
    event
}

/// The same event as the omp extension of 5.1.0 to 6.1.4 sends it: its
/// pattern, `<knives-guidance-([0-9a-f]+) repo="[^"]*">`, finds the guidance
/// blocks alone, and it sends no `notices`.
fn tool_in_old_context(path: &Path, turn: &str, held: &[&str]) -> Value {
    let guidance = held
        .iter()
        .flat_map(|text| block_nonces(text, "guidance"))
        .collect::<BTreeSet<_>>();
    let mut event = tool(path, None);
    event["context"] = json!({"turn": turn, "guidance": guidance});
    event
}

/// The envelope nonces of the `kind` blocks (`guidance` or `notice`) in a
/// tool result.
fn block_nonces<'a>(text: &'a str, kind: &str) -> Vec<&'a str> {
    let open = format!("<knives-{kind}-");
    text.lines()
        .filter_map(|line| line.strip_prefix(open.as_str()))
        .filter_map(|rest| rest.split_once(' ').map(|(nonce, _)| nonce))
        .collect()
}

/// The envelope nonces of the guidance blocks in a tool addition.
fn guidance_nonces(addition: &str) -> Vec<&str> {
    block_nonces(addition, "guidance")
}

#[test]
fn guidance_the_context_lost_is_injected_again_once() {
    // Given: a session whose first read in a trusted repository got its guidance.
    let repos = Repositories::new();
    let file = repos.trusted.join("file.txt");
    let first = run_hook(repos.home.path(), &tool_in_context(&file, "one", &[]));
    assert_eq!(guidance_nonces(addition(&first)).len(), 1, "was: {first}");

    // When: a later turn reads there while the context holds the block, then
    // two more after the harness shook it out of the context.
    let held = run_hook(
        repos.home.path(),
        &tool_in_context(&file, "two", &[addition(&first)]),
    );
    let lost = run_hook(repos.home.path(), &tool_in_context(&file, "three", &[]));
    let again = guidance_nonces(addition(&lost));
    let after = run_hook(
        repos.home.path(),
        &tool_in_context(&file, "four", &[addition(&lost)]),
    );

    // Then: the guidance comes back once, the first time it is missing.
    assert_eq!(addition(&held), "", "was: {held}");
    assert_eq!(again.len(), 1, "was: {lost}");
    assert!(
        addition(&lost).contains("trusted instructions"),
        "was: {lost}"
    );
    assert_eq!(addition(&after), "", "was: {after}");
}

#[test]
fn a_block_counts_as_held_until_its_turn_is_over() {
    // Given: two tool calls of one turn, the second made before the first's
    // result reaches the context.
    let repos = Repositories::new();
    let file = repos.trusted.join("file.txt");
    let first = run_hook(repos.home.path(), &tool_in_context(&file, "one", &[]));
    let parallel = run_hook(repos.home.path(), &tool_in_context(&file, "one", &[]));

    // When: a later turn's context does not hold the block either (its result
    // never reached the model).
    let later = run_hook(repos.home.path(), &tool_in_context(&file, "two", &[]));

    // Then: the turn delivers it once, and the next turn delivers it again.
    assert_eq!(guidance_nonces(addition(&first)).len(), 1, "was: {first}");
    assert_eq!(addition(&parallel), "", "was: {parallel}");
    assert_eq!(guidance_nonces(addition(&later)).len(), 1, "was: {later}");
}

#[test]
fn a_second_checkout_gets_the_instructions_once_their_only_block_is_lost() {
    // Given: two checkouts of one trusted repository carrying the same AGENTS.md,
    // the second read while the first's block is in the context.
    let repos = Repositories::new();
    let second = repos.home.path().join("trusted-second");
    git_repository(
        &second,
        &[("origin", "https://forge.invalid/company/trusted.git")],
    );
    std::fs::write(second.join("AGENTS.md"), "trusted instructions").expect("write instructions");
    std::fs::write(second.join("file.txt"), "content").expect("write file");
    let first = run_hook(
        repos.home.path(),
        &tool_in_context(&repos.trusted.join("file.txt"), "one", &[]),
    );
    let repeat = run_hook(
        repos.home.path(),
        &tool_in_context(&second.join("file.txt"), "two", &[addition(&first)]),
    );

    // When: the second checkout is read after that block left the context.
    let lost = run_hook(
        repos.home.path(),
        &tool_in_context(&second.join("file.txt"), "three", &[]),
    );

    // Then: the instructions arrive again, once.
    assert_eq!(addition(&repeat), "", "was: {repeat}");
    assert_eq!(guidance_nonces(addition(&lost)).len(), 1, "was: {lost}");
    assert!(
        addition(&lost).contains("trusted instructions"),
        "was: {lost}"
    );
}

#[test]
fn a_root_the_system_prompt_guided_stays_guided_whatever_the_context_holds() {
    // Given: trusted's root instructions are in the system prompt, and a
    // subdirectory has its own.
    let repos = Repositories::new();
    std::fs::create_dir_all(repos.trusted.join("sub")).expect("create subdirectory");
    std::fs::write(repos.trusted.join("sub/AGENTS.md"), "sub instructions")
        .expect("write nested instructions");
    std::fs::write(repos.trusted.join("sub/file.txt"), "content").expect("write nested file");
    let seeing = |path: &Path, turn: &str| {
        let mut event = tool_in_context(path, turn, &[]);
        event["system"] = json!(["trusted instructions"]);
        event
    };

    // When: the root is read first, then the subdirectory on a later turn
    // whose context holds no guidance block.
    let root = run_hook(
        repos.home.path(),
        &seeing(&repos.trusted.join("file.txt"), "one"),
    );
    let nested = run_hook(
        repos.home.path(),
        &seeing(&repos.trusted.join("sub/file.txt"), "two"),
    );

    // Then: the root counts as guided by its first read, as without a
    // context, so neither read injects anything.
    assert_eq!(addition(&root), "", "was: {root}");
    assert_eq!(addition(&nested), "", "was: {nested}");
}

#[test]
fn guidance_recorded_before_the_context_was_shown_is_checked_against_it() {
    // Given: a session guided by a hook call that carried no context, as from
    // an adapter predating the context field.
    let repos = Repositories::new();
    let file = repos.trusted.join("file.txt");
    let unseen = run_hook(repos.home.path(), &tool(&file, None));
    assert!(
        addition(&unseen).contains("trusted instructions"),
        "was: {unseen}"
    );

    // When: a call shows a context that holds no guidance block.
    let shown = run_hook(repos.home.path(), &tool_in_context(&file, "one", &[]));

    // Then: the guidance arrives again.
    assert!(
        addition(&shown).contains("trusted instructions"),
        "was: {shown}"
    );
}

#[test]
fn guidance_the_chat_hook_put_in_the_system_prompt_is_not_injected_again() {
    // Given: beta with a CONTRIBUTING.md, and the guidance the chat hook
    // renders into the system prompt of a session standing in beta.
    let repos = Repositories::new();
    std::fs::write(repos.beta.join("CONTRIBUTING.md"), "contribution guide")
        .expect("write contributing guide");
    let chat = run_hook(
        repos.home.path(),
        &json!({"event": "chat.system", "session_id": "chat", "directory": repos.beta}),
    );
    let system = chat["system"].as_str().expect("chat guidance");
    let mut unseen = tool(&repos.beta.join("file.txt"), None);
    unseen["session_id"] = json!("a-session-without-that-prompt");

    // When: a file in beta is read with that prompt, and in a session without it.
    let seeing = run_hook(
        repos.home.path(),
        &tool_seeing(&repos.beta.join("file.txt"), &["base prompt", system]),
    );
    let unseen = run_hook(repos.home.path(), &unseen);

    // Then: neither the instructions nor the CONTRIBUTING.md pointer arrive a
    // second time; without the prompt, both do.
    assert!(
        !addition(&seeing).contains("<knives-guidance-"),
        "was: {seeing}"
    );
    assert!(
        addition(&unseen).contains("Additional guidance exists at"),
        "was: {unseen}"
    );
}

#[cfg(unix)]
#[test]
fn a_read_only_config_home_still_delivers_guidance_and_still_leaves_out_held_text() {
    use std::os::unix::fs::PermissionsExt as _;

    // Given: a config home the hook cannot write, and a prompt carrying beta's instructions.
    let repos = Repositories::new();
    let home = repos.home.path();
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o555)).expect("chmod 555");
    // Root ignores directory permissions; there is nothing to test then.
    if std::fs::create_dir(home.join("probe")).is_ok() {
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return;
    }

    // When: a file in another trusted repository is read, then one in beta.
    let system = ["beta instructions"];
    let (trusted_ok, trusted, trusted_errors) = run_hook_input(
        home,
        &tool_seeing(&repos.trusted.join("file.txt"), &system).to_string(),
    );
    let (beta_ok, beta, beta_errors) = run_hook_input(
        home,
        &tool_seeing(&repos.beta.join("file.txt"), &system).to_string(),
    );
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");

    // Then: the unrecordable state costs no guidance, and held text stays out.
    assert!(trusted_ok && beta_ok, "{trusted_errors}{beta_errors}");
    assert!(trusted_errors.contains("knives hook:"), "{trusted_errors}");
    assert!(trusted.contains("trusted instructions"), "{trusted}");
    assert!(!beta.contains("beta instructions"), "{beta}");
}

#[test]
fn a_nested_jj_under_a_trusted_git_checkout_is_the_checkouts_content() {
    // Given: a `.jj` directory nested under a Git checkout whose remote
    // self-declares a trusted owner. A `.jj` is content a checkout can carry;
    // the nearest `.git` decides, so the nested tree gets the checkout's
    // verdict — its guidance, attributed to the checkout — and never its own
    // identity.
    let home = tempfile::tempdir().expect("config home");
    let git_root = home.path().join("parent-git");
    let nested = git_root.join("node_modules/evil");
    let initialized = std::process::Command::new("git")
        .args(["init", git_root.to_str().expect("utf-8 test path")])
        .status()
        .expect("run git init");
    assert!(initialized.success());
    let remote_added = std::process::Command::new("git")
        .args([
            "-C",
            git_root.to_str().expect("utf-8 test path"),
            "remote",
            "add",
            "origin",
            "https://forge.invalid/trusted-owner/parent.git",
        ])
        .status()
        .expect("add trusted remote");
    assert!(remote_added.success());
    std::fs::create_dir_all(nested.join(".jj")).expect("create nested pseudo-checkout");
    std::fs::write(nested.join("file.txt"), "content").expect("write file");
    std::fs::write(git_root.join("AGENTS.md"), "parent instructions").expect("write guidance");
    std::fs::write(
        home.path().join("repos.toml"),
        "[trust]\nowners = [\"trusted-owner\"]\n",
    )
    .expect("write trust config");

    // When: the hook reads the nested file.
    let output = run_hook(
        home.path(),
        &tool(
            &nested.join("file.txt"),
            Some(json!({"notice": true, "guidance": true})),
        ),
    );

    // Then: the checkout's guidance, as the checkout; no managed notice.
    let added = addition(&output);
    assert!(added.contains("repo=\"parent-git\""), "{added}");
    assert!(added.contains("parent instructions"), "{added}");
    assert!(!added.contains("<knives-notice-"), "{added}");
}

#[test]
fn a_git_root_with_a_trusted_origin_injects_guidance() {
    // Given: a real Git checkout whose own origin claims a trusted owner.
    let home = tempfile::tempdir().expect("config home");
    let root = home.path().join("trusted-git");
    let initialized = std::process::Command::new("git")
        .args(["init", root.to_str().expect("utf-8 test path")])
        .status()
        .expect("run git init");
    assert!(initialized.success());
    let remote_added = std::process::Command::new("git")
        .args([
            "-C",
            root.to_str().expect("utf-8 test path"),
            "remote",
            "add",
            "origin",
            "https://forge.invalid/trusted-owner/repo.git",
        ])
        .status()
        .expect("add trusted remote");
    assert!(remote_added.success());
    std::fs::write(root.join("AGENTS.md"), "trusted owner instructions").expect("write guidance");
    std::fs::write(root.join("file.txt"), "content").expect("write file");
    std::fs::write(
        home.path().join("repos.toml"),
        "[trust]\nowners = [\"trusted-owner\"]\n",
    )
    .expect("write trust config");

    // When: the hook reads a file in that checkout.
    let output = run_hook(
        home.path(),
        &tool(
            &root.join("file.txt"),
            Some(json!({"notice": true, "guidance": true})),
        ),
    );

    // Then: the documented self-declared-owner grant injects guidance only.
    assert!(
        addition(&output).contains("<knives-guidance-"),
        "was: {output}"
    );
    assert!(
        !addition(&output).contains("<knives-notice-"),
        "was: {output}"
    );
}

#[test]
fn chat_system_returns_formatted_guidance_and_raw_bodies() {
    // Given: a repository whose root has instructions.
    let repos = Repositories::new();
    let event = json!({"event": "chat.system", "session_id": SESSION_ID, "directory": repos.beta});

    // When: OpenCode requests its system context.
    let output = run_hook(repos.home.path(), &event);

    // Then: the shim receives both its formatted insertion and machine-readable bodies.
    assert!(
        output["system"]
            .as_str()
            .is_some_and(|text| text.contains("<knives-guidance-"))
    );
    assert_eq!(output["bodies"], json!(["beta instructions"]));
}

#[test]
fn chat_system_returns_guidance_for_a_trusted_directory() {
    // Given: a trusted repository whose root has instructions.
    let repos = Repositories::new();
    let event =
        json!({"event": "chat.system", "session_id": SESSION_ID, "directory": repos.trusted});

    // When: OpenCode requests its system context.
    let output = run_hook(repos.home.path(), &event);

    // Then: trusted guidance has the same system response shape as managed guidance.
    assert!(
        output["system"]
            .as_str()
            .is_some_and(|text| text.contains("<knives-guidance-"))
    );
    assert_eq!(output["bodies"], json!(["trusted instructions"]));
}

#[test]
fn shell_env_exports_its_event_session_never_a_claim_owner() {
    // Given: a fresh shell event under a managed repository with an existing foreign claim.
    let repos = Repositories::new();
    let event = json!({
        "event": "shell.env",
        "session_id": "fresh-opencode-session",
        "cwd": repos.beta
    });

    // When: OpenCode requests its shell environment.
    let output = run_hook(repos.home.path(), &event);

    // Then: start receives the fresh harness identity, not the stored claim holder.
    assert_eq!(output, json!({"owner": "fresh-opencode-session"}));
}

#[test]
fn shell_env_never_exports_a_claim_owner_for_a_trusted_repo() {
    // Given: a trusted root with a claim that otherwise looks owner-exportable.
    let repos = Repositories::new();
    repos.write_state(&json!({"claims": {"trusted/feat/claimed": {
        "repo": "trusted", "branch": "feat/claimed", "owner": "attacker",
        "why": "claim", "started": "2026-01-01T00:00:00Z", "files": []
    }}}));

    // When: OpenCode requests shell ownership for the trusted root.
    let output = run_hook(
        repos.home.path(),
        &json!({"event": "shell.env", "cwd": repos.trusted}),
    );

    // Then: trusted guidance roots do not acquire managed owner exports.
    assert_eq!(output, json!({"owner": null}));
}

#[test]
fn shell_env_without_an_event_session_exports_no_owner() {
    // An inherited shell variable or stored claim cannot create a harness identity.
    let repos = Repositories::new();
    let event = json!({"event": "shell.env", "cwd": repos.beta});

    let output = run_hook_with_owner(repos.home.path(), &event, Some("inherited-owner"));

    assert_eq!(output, json!({"owner": null}));
}

#[test]
fn shell_env_returns_no_owner_for_distinct_claim_owners() {
    // Given: a managed root with claims held by two different owners.
    let repos = Repositories::new();
    repos.write_state(&json!({"claims": {
        "maintainer/beta/feat/one": {
            "repo": "maintainer/beta", "branch": "feat/one", "owner": "agent-one",
            "why": "one", "started": "2026-01-01T00:00:00Z", "files": []
        },
        "maintainer/beta/feat/two": {
            "repo": "maintainer/beta", "branch": "feat/two", "owner": "agent-two",
            "why": "two", "started": "2026-01-01T00:00:00Z", "files": []
        }
    }}));
    let event = json!({"event": "shell.env", "cwd": repos.beta});

    // When: OpenCode requests the owner.
    let output = run_hook(repos.home.path(), &event);

    // Then: an ambiguous claim set does not select an owner.
    assert_eq!(output, json!({"owner": null}));
}

#[test]
fn compacting_resets_the_tool_after_budget() {
    // Given: a session that has spent its managed-repository budget.
    let repos = Repositories::new();
    let event = tool(&repos.beta.join("file.txt"), None);
    let first = run_hook(repos.home.path(), &event);

    // When: compaction clears the session before another read.
    let compacted = run_hook(
        repos.home.path(),
        &json!({"event": "compacting", "session_id": SESSION_ID}),
    );
    let second = run_hook(repos.home.path(), &event);

    // Then: compaction is empty and the full addition is available again.
    assert_eq!(compacted, json!({}));
    assert!(addition(&first).contains("<knives-notice-"));
    assert!(addition(&second).contains("<knives-notice-"));
    assert!(addition(&second).contains("beta instructions"));
}

#[test]
fn malformed_input_returns_an_empty_response_without_failing() {
    // Given: malformed hook input.
    let home = tempfile::tempdir().expect("config home");

    // When: it reaches the hook binary.
    let (success, malformed, errors) = run_hook_input(home.path(), "not json");

    // Then: it cannot interrupt OpenCode and has an empty envelope.
    assert!(success);
    assert_eq!(
        serde_json::from_str::<Value>(&malformed).expect("empty JSON response"),
        json!({})
    );
    assert!(!errors.is_empty(), "malformed input is reported on stderr");
}

#[test]
fn unreadable_stdin_returns_an_empty_response_without_failing() {
    // Given: a hook whose standard input is a directory rather than readable event data.
    let home = tempfile::tempdir().expect("config home");
    let stdin = File::open(home.path()).expect("open config directory");

    // When: OpenCode invokes the hook.
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["hook", "opencode"])
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .stdin(Stdio::from(stdin))
        .output()
        .expect("run hook");

    // Then: the hook reports the read failure but preserves OpenCode's empty envelope.
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}");
    assert!(
        !output.stderr.is_empty(),
        "stdin failure is reported on stderr"
    );
}

#[test]
fn pathless_tool_after_skips_the_registry_without_stderr() {
    // Given: a pathless Bash event and a malformed registry.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "[[[garbage").expect("write malformed registry");
    let event = json!({"event": "tool.execute.after", "session_id": SESSION_ID, "tool": "bash", "args": {}});

    // When: the event reaches the hook binary.
    let (success, output, errors) = run_hook_input(home.path(), &event.to_string());

    // Then: the pathless fast path returns its envelope without loading the registry.
    assert!(success);
    assert_eq!(output, r#"{"addition":""}"#);
    assert!(
        errors.is_empty(),
        "pathless fast path must stay quiet: {errors}"
    );
}

#[test]
fn pathful_tool_after_returns_an_empty_envelope_when_the_registry_is_malformed() {
    // Given: a pathful tool event and a malformed registry.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "[[[garbage").expect("write malformed registry");
    let event = tool(&home.path().join("named.txt"), None);

    // When: the event reaches the hook binary.
    let (success, output, errors) = run_hook_input(home.path(), &event.to_string());

    // Then: the tool envelope remains parseable and the error is on stderr.
    assert!(success);
    assert_eq!(output, r#"{"addition":""}"#);
    assert!(!errors.is_empty(), "registry failure is reported on stderr");
}

#[test]
fn chat_system_returns_an_empty_envelope_when_the_registry_is_malformed() {
    // Given: a chat-system event and a malformed registry.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "[[[garbage").expect("write malformed registry");
    let event = json!({"event": "chat.system", "directory": home.path()});

    // When: OpenCode requests system context.
    let (success, output, errors) = run_hook_input(home.path(), &event.to_string());

    // Then: its system envelope remains parseable and the error is on stderr.
    assert!(success);
    assert_eq!(output, r#"{"system":"","bodies":[]}"#);
    assert!(!errors.is_empty(), "registry failure is reported on stderr");
}

#[test]
fn shell_env_ignores_a_malformed_registry() {
    // Given: a shell-environment event and a malformed registry.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "[[[garbage").expect("write malformed registry");
    let event = json!({"event": "shell.env", "cwd": home.path()});

    // When: the event reaches the hook binary.
    let (success, output, errors) = run_hook_input(home.path(), &event.to_string());

    // Then: shell owner comes only from the event session, never registry state.
    assert!(success);
    assert_eq!(output, r#"{"owner":null}"#);
    assert!(errors.is_empty(), "stderr: {errors}");
}

#[test]
fn shell_env_ignores_malformed_state() {
    // Given: a managed shell-environment event and malformed state.
    let repos = Repositories::new();
    std::fs::write(repos.home.path().join("state.json"), "[[[garbage")
        .expect("write malformed state");
    let event = json!({"event": "shell.env", "cwd": repos.beta});

    // When: the event reaches the hook binary.
    let (success, output, errors) = run_hook_input(repos.home.path(), &event.to_string());

    // Then: shell owner comes only from the event session, never persisted state.
    assert!(success);
    assert_eq!(output, r#"{"owner":null}"#);
    assert!(errors.is_empty(), "stderr: {errors}");
}

#[test]
fn compacting_returns_an_empty_envelope_when_its_state_path_is_invalid() {
    // Given: compaction whose session-state directory is a regular file.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("hook-sessions"), "not a directory")
        .expect("write invalid state path");
    let event = json!({"event": "compacting", "session_id": SESSION_ID});

    // When: OpenCode compacts the session.
    let (success, output, errors) = run_hook_input(home.path(), &event.to_string());

    // Then: it receives the empty envelope while the state error is reported on stderr.
    assert!(success);
    assert_eq!(output, "{}");
    assert!(!errors.is_empty(), "state failure is reported on stderr");
}

#[test]
fn an_abandoned_hook_invocation_exits_at_its_deadline_instead_of_living_forever() {
    // Given: a harness spawned the hook with a piped stdin and then abandoned
    // it — nothing will ever write or close that pipe. This is the state that
    // accumulated ~13k immortal knives processes and took down a devbox on
    // 2026-08-25: without a watchdog the process blocks in its stdin read.
    let mut child = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["hook", "opencode"])
        .env("KNIVES_HOOK_DEADLINE_MS", "250")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");

    // When: the deadline passes. Poll rather than block, so a regression fails
    // the test instead of hanging the suite.
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll hook") {
            break status;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "hook process outlived its watchdog deadline"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    };

    // Then: the watchdog ended the process — Incomplete (3), never clap's
    // usage code (2), which harnesses read as "binary too old".
    assert_eq!(status.code(), Some(3), "watchdog exit code");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("hook stderr")
        .read_to_string(&mut stderr)
        .expect("read hook stderr");
    assert!(
        stderr.contains("gave up after 250ms"),
        "stderr names the deadline: {stderr}"
    );
}
