#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unreachable,
    reason = "fixture setup failures and JSON shape mismatches are test failures"
)]
// allow: SIZE_OK: 997 lines - real-binary adapter scenarios share one fixture and process harness.

#[path = "common/lab.rs"]
mod lab;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use lab::{git_repository, jj_checkout};
use serde_json::{Value, json};

const SESSION_ID: &str = "hook-test-session";

fn fixture(name: &str) -> Value {
    serde_json::from_str(match name {
        "session-start" => include_str!("fixtures/claude_hook_session_start.json"),
        "post-tool-read" => include_str!("fixtures/claude_hook_post_tool_use_read.json"),
        "post-tool-bash" => include_str!("fixtures/claude_hook_post_tool_use_bash.json"),
        "pre-compact" => include_str!("fixtures/claude_hook_pre_compact_constructed.json"),
        "session-end" => include_str!("fixtures/claude_hook_session_end.json"),
        _ => unreachable!("known hook fixture"),
    })
    .expect("fixture is valid JSON")
}

fn event(name: &str, cwd: &Path, path: Option<&Path>) -> Value {
    let mut event = fixture(name);
    event["session_id"] = json!(SESSION_ID);
    if let Some(object) = event.as_object_mut() {
        object.insert("cwd".to_owned(), json!(cwd));
    }
    if let Some(path) = path {
        event["tool_input"]["file_path"] = json!(path);
    }
    event
}

fn hook_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["hook", "claude-code"]);
    command
        .env("KNIVES_CONFIG_HOME", home)
        .env("HOME", home)
        .env("JJ_CONFIG", "/dev/null");
    command
}

fn run_command_input(mut command: Command, input: &str) -> (bool, String, String) {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
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

fn run_hook_input(home: &Path, input: &str) -> (bool, String, String) {
    run_command_input(hook_command(home), input)
}

fn run_hook(home: &Path, event: &Value) -> String {
    let (success, output, errors) = run_hook_input(home, &event.to_string());
    assert!(success, "a hook must never fail the session: {errors}");
    output
}

struct Repositories {
    home: tempfile::TempDir,
    /// Managed AND trusted: `origin` sits under a trusted owner.
    alpha: PathBuf,
    /// Managed, NOT trusted: `origin` belongs to a stranger.
    beta: PathBuf,
    /// Trusted only, through `[trust] repos`.
    trusted: PathBuf,
}

impl Repositories {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("config home");
        let alpha = home.path().join("alpha");
        let beta = home.path().join("beta");
        let trusted = home.path().join("trusted");
        git_repository(
            &alpha,
            &[
                ("upstream", "https://forge.invalid/maintainer/alpha"),
                ("origin", "https://forge.invalid/ours/alpha"),
            ],
        );
        git_repository(
            &beta,
            &[
                ("upstream", "https://forge.invalid/maintainer/beta"),
                ("origin", "https://forge.invalid/stranger/beta"),
            ],
        );
        git_repository(
            &trusted,
            &[("origin", "https://forge.invalid/company/trusted.git")],
        );
        for (root, instructions) in [
            (&alpha, "alpha instructions"),
            (&beta, "beta instructions"),
            (&trusted, "trusted instructions"),
        ] {
            std::fs::write(root.join("AGENTS.md"), instructions).expect("write instructions");
            std::fs::write(root.join("file.txt"), "content").expect("write file");
        }
        Self {
            home,
            alpha,
            beta,
            trusted,
        }
    }

    fn configure(&self, include_trusted: bool) {
        let trust = if include_trusted {
            "[trust]\nowners = [\"ours\"]\nrepos = [\"company/trusted\"]\n"
        } else {
            "[trust]\nowners = [\"ours\"]\n"
        };
        let config = format!(
            "[repos.alpha]\nupstream = \"https://forge.invalid/maintainer/alpha\"\n\
             origin = \"https://forge.invalid/ours/alpha\"\n\n\
             [repos.beta]\nupstream = \"https://forge.invalid/maintainer/beta\"\n\
             origin = \"https://forge.invalid/ours/beta\"\n\n{trust}"
        );
        std::fs::write(self.home.path().join("repos.toml"), config).expect("write registry");
        let state = json!({"claims": {"beta/feat/claimed": {
            "repo": "beta",
            "branch": "feat/claimed",
            "owner": "agent-one",
            "why": "porting",
            "started": "2026-01-01T00:00:00Z",
            "files": []
        }}});
        std::fs::write(self.home.path().join("state.json"), state.to_string())
            .expect("write state");
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

fn additional_context(output: &str) -> String {
    serde_json::from_str::<Value>(output)
        .expect("hook output JSON")["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additional context")
        .to_owned()
}

fn response_event_name(output: &str) -> String {
    serde_json::from_str::<Value>(output)
        .expect("hook output JSON")["hookSpecificOutput"]["hookEventName"]
        .as_str()
        .expect("hook event name")
        .to_owned()
}

#[test]
fn session_start_inside_a_managed_repo_emits_the_notice_with_claims() {
    // Given: a managed repository with instructions and an active claim.
    let repos = Repositories::new();
    repos.configure(false);
    let start = event("session-start", &repos.beta, None);

    // When: Claude Code starts inside it.
    let output = run_hook(repos.home.path(), &start);

    // Then: the notice names the claim but does not duplicate native guidance.
    let context = additional_context(&output);
    assert_eq!(
        response_event_name(&output),
        start["hook_event_name"].as_str().expect("event name")
    );
    assert!(context.contains("fork managed by knives"), "was: {context}");
    assert!(
        context.contains("feat/claimed (agent-one, os-user, claimed "),
        "was: {context}"
    );
    assert!(
        context.contains("not seen within the observation window): porting"),
        "was: {context}"
    );
    assert!(!context.contains("beta instructions"), "was: {context}");
}

#[test]
fn session_start_reemits_a_notice_when_the_roster_changes() {
    // A one-time boolean must not hide a new claim when a session starts again
    // in a repository whose roster has changed.
    let repos = Repositories::new();
    repos.configure(false);
    let start = event("session-start", &repos.beta, None);

    let _ = run_hook(repos.home.path(), &start);
    repos.write_state(&json!({"claims": {
        "beta/feat/claimed": {
            "repo": "beta",
            "branch": "feat/claimed",
            "owner": "agent-one",
            "why": "porting",
            "started": "2026-01-01T00:00:00Z",
            "files": []
        },
        "beta/feat/new": {
            "repo": "beta",
            "branch": "feat/new",
            "owner": "agent-two",
            "why": "reviewing",
            "started": "2026-01-02T00:00:00Z",
            "files": []
        }
    }}));
    let second = run_hook(repos.home.path(), &start);

    assert!(!second.is_empty(), "a changed roster must be noticed");
    assert!(
        additional_context(&second).contains("feat/new"),
        "was: {second}"
    );
}

#[test]
fn session_start_in_a_managed_workspace_records_passive_observations() {
    let repos = Repositories::new();
    repos.configure(false);
    Repositories::colocate(&repos.beta);
    let start = event("session-start", &repos.beta, None);

    let _ = run_hook(repos.home.path(), &start);

    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(repos.home.path().join("seen.json"))
            .expect("SessionStart records seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["owners"]["harness-session"][SESSION_ID]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
    assert!(
        seen["workspaces"]["beta/beta"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
}

#[test]
fn compact_session_start_in_a_managed_workspace_records_passive_observations() {
    let repos = Repositories::new();
    repos.configure(false);
    Repositories::colocate(&repos.beta);
    let mut compact = event("session-start", &repos.beta, None);
    compact["source"] = json!("compact");

    assert!(run_hook(repos.home.path(), &compact).is_empty());

    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(repos.home.path().join("seen.json"))
            .expect("compact SessionStart records seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["owners"]["harness-session"][SESSION_ID]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
    assert!(
        seen["workspaces"]["beta/beta"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
}

#[test]
fn compact_session_start_resets_the_notice_budget() {
    // Given: a managed session has already received its notice.
    let repos = Repositories::new();
    repos.configure(false);
    let start = event("session-start", &repos.beta, None);
    let mut compact_restart = event("session-start", &repos.beta, None);
    compact_restart["source"] = json!("compact");
    let first = run_hook(repos.home.path(), &start);

    // When: SessionStart reports that it was caused by compaction.
    let compact = run_hook(repos.home.path(), &compact_restart);
    let restarted = run_hook(repos.home.path(), &start);

    // Then: compaction emits nothing and the restarted session receives a new notice.
    assert!(additional_context(&first).contains("fork managed by knives"));
    assert!(compact.is_empty(), "was: {compact}");
    assert!(additional_context(&restarted).contains("fork managed by knives"));
}

#[test]
fn session_start_inside_a_trusted_root_emits_nothing() {
    // Given: Claude Code starts in a trusted root.
    let repos = Repositories::new();
    repos.configure(true);
    let start = event("session-start", &repos.trusted, None);

    // When: the adapter receives the start event.
    let output = run_hook(repos.home.path(), &start);

    // Then: only managed roots receive a SessionStart notice.
    assert!(output.is_empty(), "was: {output}");
}

#[test]
fn an_unknown_hook_event_emits_nothing() {
    // Given: an event name the adapter does not recognize inside a managed repository.
    let repos = Repositories::new();
    repos.configure(false);
    let mut unknown = event("session-start", &repos.beta, None);
    unknown["hook_event_name"] = json!("UnknownEvent");

    // When: it reaches the hook binary.
    let output = run_hook(repos.home.path(), &unknown);

    // Then: unsupported events are ignored.
    assert!(output.is_empty(), "was: {output}");
}

#[test]
fn post_tool_use_on_a_foreign_repo_emits_notice_and_guidance_once() {
    // Given: a session in beta that reads a file in alpha, a fork that is both
    // managed and trusted.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.beta,
        Some(&repos.alpha.join("file.txt")),
    );

    // When: the same foreign file is read twice.
    let first = run_hook(repos.home.path(), &read);
    let second = run_hook(repos.home.path(), &read);

    // Then: only the first read carries alpha's notice and guidance.
    let context = additional_context(&first);
    assert_eq!(
        response_event_name(&first),
        read["hook_event_name"].as_str().expect("event name")
    );
    assert!(context.contains("fork managed by knives"), "was: {context}");
    assert!(context.contains("alpha instructions"), "was: {context}");
    assert!(second.is_empty(), "was: {second}");
}

#[test]
fn post_tool_use_reemits_a_notice_when_the_roster_changes() {
    // A changed roster must be reported on the foreign-repository path too,
    // rather than only at SessionStart.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );

    let _ = run_hook(repos.home.path(), &read);
    repos.write_state(&json!({"claims": {
        "beta/feat/claimed": {
            "repo": "beta",
            "branch": "feat/claimed",
            "owner": "agent-one",
            "why": "porting",
            "started": "2026-01-01T00:00:00Z",
            "files": []
        },
        "beta/feat/new": {
            "repo": "beta",
            "branch": "feat/new",
            "owner": "agent-two",
            "why": "reviewing",
            "started": "2026-01-02T00:00:00Z",
            "files": []
        }
    }}));
    let second = run_hook(repos.home.path(), &read);

    assert!(
        additional_context(&second).contains("feat/new"),
        "was: {second}"
    );
}

#[test]
fn post_tool_use_in_a_managed_workspace_records_event_identity_and_cwd() {
    let repos = Repositories::new();
    repos.configure(false);
    Repositories::colocate(&repos.alpha);
    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );

    let _ = run_hook(repos.home.path(), &read);

    let seen: Value = serde_json::from_str(
        &std::fs::read_to_string(repos.home.path().join("seen.json"))
            .expect("PostToolUse records seen.json"),
    )
    .expect("seen JSON");
    assert!(
        seen["owners"]["harness-session"][SESSION_ID]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
    assert!(
        seen["workspaces"]["alpha/alpha"]
            .as_str()
            .is_some_and(|timestamp| timestamp.parse::<jiff::Timestamp>().is_ok()),
        "was: {seen}"
    );
}

#[test]
fn a_hook_whose_own_working_directory_has_vanished_still_answers() {
    // The hook process inherits the harness's cwd, which a finished workspace
    // can take away under it. The hook never binds its own cwd — the event
    // carries the one that matters — so it must answer as if nothing happened.
    let repos = Repositories::new();
    repos.configure(false);
    // A shell started in a fresh directory removes it from under itself, then
    // becomes knives with that vanished cwd.
    let from_vanished_cwd = |event: &Value| {
        let vanishing = tempfile::tempdir_in(repos.home.path()).expect("directory to vanish");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                r#"rmdir "$0" && exec "$1" hook claude-code"#,
                vanishing.path().to_str().expect("utf-8 path"),
                env!("CARGO_BIN_EXE_knives"),
            ])
            .current_dir(vanishing.path())
            .env("KNIVES_CONFIG_HOME", repos.home.path())
            .env("HOME", repos.home.path())
            .env("JJ_CONFIG", "/dev/null");
        let answer = run_command_input(command, &event.to_string());
        assert!(
            !vanishing.path().exists(),
            "the fixture must have taken the cwd away"
        );
        // Already gone; keep `TempDir`'s drop from reporting it.
        let _ = vanishing.keep();
        answer
    };

    let (success, output, errors) = from_vanished_cwd(&event("post-tool-bash", &repos.alpha, None));
    assert!(success, "a hook must never fail the session: {errors}");
    assert!(output.is_empty(), "was: {output}");
    assert!(errors.is_empty(), "was: {errors}");

    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );
    let (success, output, errors) = from_vanished_cwd(&read);
    assert!(success, "a hook must never fail the session: {errors}");
    assert!(
        additional_context(&output).contains("beta"),
        "the event's own cwd still drives the notice: {output}\n{errors}"
    );
    assert!(errors.is_empty(), "was: {errors}");
}

#[test]
fn post_tool_use_without_cwd_emits_notice_without_guidance() {
    // Given: a managed, trusted file in a PostToolUse event that has no session cwd.
    let repos = Repositories::new();
    repos.configure(false);
    let mut read = event(
        "post-tool-read",
        &repos.beta,
        Some(&repos.alpha.join("file.txt")),
    );
    read.as_object_mut().expect("event object").remove("cwd");

    // When: the foreign file is read.
    let context = additional_context(&run_hook(repos.home.path(), &read));

    // Then: the managed-repository notice remains but repository guidance is omitted.
    assert!(context.contains("fork managed by knives"), "was: {context}");
    assert!(!context.contains("alpha instructions"), "was: {context}");
}

#[test]
fn post_tool_use_on_the_session_repo_never_injects_its_guidance() {
    // Given: a session that reads a file in its own repository.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.alpha.join("file.txt")),
    );

    // When: Claude Code reads its own repository content.
    let output = run_hook(repos.home.path(), &read);

    // Then: its native instructions are never injected again.
    assert!(!output.contains("alpha instructions"), "was: {output}");
}

#[test]
fn compaction_resets_the_budget() {
    // Given: a foreign trusted repository was announced once.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.beta,
        Some(&repos.alpha.join("file.txt")),
    );
    let compact = event("pre-compact", &repos.beta, None);
    let first = run_hook(repos.home.path(), &read);

    // When: Claude Code compacts its context and reads that repository again.
    assert!(run_hook(repos.home.path(), &compact).is_empty());
    let second = run_hook(repos.home.path(), &read);

    // Then: both reads receive the full foreign guidance.
    assert!(additional_context(&first).contains("alpha instructions"));
    assert!(additional_context(&second).contains("alpha instructions"));
}

#[test]
fn session_end_deletes_the_state_file() {
    // Given: SessionStart created a session-state record.
    let repos = Repositories::new();
    repos.configure(false);
    let start = event("session-start", &repos.alpha, None);
    let end = event("session-end", &repos.alpha, None);
    let _ = run_hook(repos.home.path(), &start);

    // When: Claude Code ends the session.
    assert!(run_hook(repos.home.path(), &end).is_empty());

    // Then: no session state remains for the session.
    let sessions = repos.home.path().join("hook-sessions");
    assert!(
        std::fs::read_dir(sessions)
            .expect("session directory")
            .next()
            .is_none()
    );
}

#[test]
fn malformed_input_yields_empty_output_and_exit_zero() {
    // Given: malformed hook input.
    let home = tempfile::tempdir().expect("config home");

    // When: it reaches the hook binary.
    let (success, output, _) = run_hook_input(home.path(), "not json");

    // Then: it cannot interrupt the user's session or produce a response.
    assert!(success);
    assert!(output.is_empty(), "was: {output}");
}

#[test]
fn irrelevant_tools_are_ignored() {
    // Given: an unsupported tool that names a managed path.
    let repos = Repositories::new();
    repos.configure(false);
    let mut read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );
    read["tool_name"] = json!("WebFetch");

    // When: the unsupported tool completes.
    let output = run_hook(repos.home.path(), &read);

    // Then: it emits nothing.
    assert!(output.is_empty(), "was: {output}");
}

#[test]
fn a_trusted_repo_gets_guidance_but_never_the_notice() {
    // Given: a trusted repository read from outside its root.
    let repos = Repositories::new();
    repos.configure(true);
    let read = event(
        "post-tool-read",
        repos.home.path(),
        Some(&repos.trusted.join("file.txt")),
    );

    // When: Claude Code reads its instructions.
    let output = run_hook(repos.home.path(), &read);

    // Then: it gets only the trusted repository's guidance.
    let context = additional_context(&output);
    assert!(context.contains("trusted instructions"), "was: {context}");
    assert!(
        !context.contains("fork managed by knives"),
        "was: {context}"
    );
}

#[test]
fn missing_guidance_does_not_consume_the_session_budget() {
    // Given: a managed, trusted repository initially has no instruction file.
    let repos = Repositories::new();
    repos.configure(false);
    std::fs::remove_file(repos.alpha.join("AGENTS.md")).expect("remove instructions");
    let read = event(
        "post-tool-read",
        &repos.beta,
        Some(&repos.alpha.join("file.txt")),
    );

    // When: the notice emitted on the first read, then instructions appear.
    let first = run_hook(repos.home.path(), &read);
    assert!(additional_context(&first).contains("fork managed by knives"));
    std::fs::write(repos.alpha.join("AGENTS.md"), "later instructions")
        .expect("write instructions");
    let output = run_hook(repos.home.path(), &read);

    // Then: the later guidance remains injectable.
    assert!(additional_context(&output).contains("later instructions"));
}

#[test]
fn a_deleted_session_cwd_does_not_trigger_own_repo_guidance() {
    // Given: a session whose former cwd was inside a managed repository.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.alpha.join("deleted/subdirectory"),
        Some(&repos.alpha.join("file.txt")),
    );

    // When: Claude Code reports a read from that repository.
    let output = run_hook(repos.home.path(), &read);

    // Then: native guidance is not duplicated even though the leaf no longer exists.
    assert!(
        !additional_context(&output).contains("alpha instructions"),
        "was: {output}"
    );
}

#[test]
fn pathless_calls_exit_before_touching_the_registry() {
    // Given: a malformed registry and a Bash event with no named path.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "[[[garbage").expect("write malformed registry");
    let bash = event("post-tool-bash", home.path(), None);

    // When: the pathless Bash event reaches the hook.
    let (success, output, errors) = run_hook_input(home.path(), &bash.to_string());

    // Then: it returns silently before attempting to parse the registry.
    assert!(success, "a hook must never fail the session");
    assert!(output.is_empty(), "was: {output}");
    assert!(errors.is_empty(), "was: {errors}");
}

/// A `Read` of `path` from a session whose cwd is the config home, so the file's
/// repository is foreign to the session and eligible for guidance.
fn post_tool_use_read(home: &Path, path: &Path, session_id: &str) -> Value {
    let mut read = fixture("post-tool-read");
    read["session_id"] = json!(session_id);
    read["cwd"] = json!(home);
    read["tool_input"]["file_path"] = json!(path);
    read
}

#[test]
fn a_managed_checkout_outside_trust_gets_the_notice_but_no_guidance() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let event = post_tool_use_read(
        repositories.home.path(),
        &repositories.beta.join("file.txt"),
        "session-beta",
    );
    let output = run_hook(repositories.home.path(), &event);
    let context = additional_context(&output);
    assert!(context.contains("fork managed by knives"), "{context}");
    assert!(!context.contains("beta instructions"), "{context}");
}

#[test]
fn a_trusted_checkout_that_is_not_a_fork_gets_guidance_but_no_notice() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let event = post_tool_use_read(
        repositories.home.path(),
        &repositories.trusted.join("file.txt"),
        "session-trusted",
    );
    let output = run_hook(repositories.home.path(), &event);
    let context = additional_context(&output);
    assert!(context.contains("trusted instructions"), "{context}");
    assert!(!context.contains("fork managed by knives"), "{context}");
}

#[test]
fn a_checkout_that_is_both_managed_and_trusted_gets_notice_and_guidance() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let event = post_tool_use_read(
        repositories.home.path(),
        &repositories.alpha.join("file.txt"),
        "session-alpha",
    );
    let output = run_hook(repositories.home.path(), &event);
    let context = additional_context(&output);
    assert!(context.contains("fork managed by knives"), "{context}");
    assert!(context.contains("alpha instructions"), "{context}");
}

#[test]
fn a_plain_git_clone_under_a_trusted_owner_gets_guidance_wherever_it_is() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let clone = repositories.home.path().join("scratch").join("tmp-clone");
    git_repository(&clone, &[("origin", "https://forge.invalid/ours/anything")]);
    std::fs::write(clone.join("AGENTS.md"), "clone instructions").expect("write");
    std::fs::write(clone.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(
        repositories.home.path(),
        &clone.join("file.txt"),
        "session-clone",
    );
    let output = run_hook(repositories.home.path(), &event);
    assert!(additional_context(&output).contains("clone instructions"));
}

/// A jj workspace of a trusted repository: guidance comes from the workspace's own tree.
#[test]
fn a_workspace_of_a_trusted_repository_gets_its_own_guidance() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let checkout = repositories.home.path().join("tool");
    jj_checkout(
        &checkout,
        &[("origin", "https://forge.invalid/company/trusted")],
    );
    std::fs::write(checkout.join("AGENTS.md"), "trusted instructions").expect("write");
    lab::jj(&checkout, ["describe", "-m", "init"]);
    lab::jj(&checkout, ["new"]);
    let workspace = repositories.home.path().join("tool-feat");
    lab::jj_workspace_add(&checkout, "feat", &workspace);
    std::fs::write(workspace.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(
        repositories.home.path(),
        &workspace.join("file.txt"),
        "session-ws",
    );
    let output = run_hook(repositories.home.path(), &event);
    assert!(
        additional_context(&output).contains("trusted instructions"),
        "{output}"
    );
}

/// Remotes that cannot be read are reported, and a `roots` rule still grants guidance.
#[test]
fn unreadable_remotes_are_reported_and_a_trust_root_still_grants_guidance() {
    let repositories = Repositories::new();
    let fake = repositories.home.path().join("under-root").join("fake");
    std::fs::create_dir_all(fake.join(".git")).expect("empty .git git cannot read");
    std::fs::write(fake.join("AGENTS.md"), "root instructions").expect("write");
    std::fs::write(fake.join("file.txt"), "content").expect("write");
    std::fs::write(
        repositories.home.path().join("repos.toml"),
        format!(
            "[trust]\nroots = [\"{}\"]\n",
            repositories.home.path().join("under-root").display()
        ),
    )
    .expect("registry");
    let event = post_tool_use_read(
        repositories.home.path(),
        &fake.join("file.txt"),
        "session-fake",
    );
    let (success, output, errors) = run_hook_input(repositories.home.path(), &event.to_string());
    assert!(success);
    assert!(errors.contains("knives hook:"), "{errors}");
    assert!(
        additional_context(&output).contains("root instructions"),
        "{output}"
    );
}

/// Session state that cannot be written is reported, never a reason to lose
/// the match: a `roots` grant needs neither jj nor a writable config home.
#[cfg(unix)]
#[test]
fn a_read_only_config_home_still_delivers_guidance_from_a_trusted_root() {
    use std::os::unix::fs::PermissionsExt as _;

    let repositories = Repositories::new();
    let elsewhere = tempfile::tempdir().expect("trusted root outside the config home");
    let clone = elsewhere.path().join("clone");
    git_repository(&clone, &[]);
    std::fs::write(clone.join("AGENTS.md"), "root instructions").expect("write");
    std::fs::write(clone.join("file.txt"), "content").expect("write");
    std::fs::write(
        repositories.home.path().join("repos.toml"),
        format!("[trust]\nroots = [\"{}\"]\n", elsewhere.path().display()),
    )
    .expect("registry");
    let home = repositories.home.path();
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o555)).expect("chmod 555");
    // Root ignores directory permissions; there is nothing to test then.
    if std::fs::create_dir(home.join("probe")).is_ok() {
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        return;
    }
    let event = post_tool_use_read(home, &clone.join("file.txt"), "session-read-only");
    let (success, output, errors) = run_hook_input(home, &event.to_string());
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");
    assert!(success, "{errors}");
    assert!(errors.contains("knives hook:"), "{errors}");
    assert!(
        additional_context(&output).contains("root instructions"),
        "{output}\n{errors}"
    );
}

/// The jj `checkout` working-copy record naming workspace `default` at jj's
/// root operation (64 zero bytes) — what a forger who knows nothing about a
/// checkout can write, since every jj store contains that operation.
fn root_operation_checkout_record() -> Vec<u8> {
    let mut record = vec![0x12, 0x40];
    record.extend(std::iter::repeat_n(0u8, 64));
    record.extend([0x1a, 0x07]);
    record.extend(b"default");
    record
}

/// A tree carrying `.jj` content that names a real, trusted, colocated
/// checkout — a `.jj/repo` pointer file alone, and one beside a hand-written
/// `.jj/working_copy` at jj's root operation — with no `.git` of its own. A
/// `.jj` without `.git` is not a repository to knives: nothing is read, nothing
/// is said, nothing is earned.
#[test]
fn a_jj_without_a_git_earns_nothing_whatever_its_pointer_says() {
    let repositories = Repositories::new();
    repositories.configure(true);
    Repositories::colocate(&repositories.alpha);
    let pointer = repositories.alpha.join(".jj").join("repo");
    let pointer = pointer.to_str().expect("utf-8");
    let bare = repositories.home.path().join("bare-pointer");
    std::fs::create_dir_all(bare.join(".jj")).expect(".jj");
    std::fs::write(bare.join(".jj").join("repo"), pointer).expect("pointer");
    let forged = repositories.home.path().join("forged-state");
    std::fs::create_dir_all(forged.join(".jj").join("working_copy")).expect(".jj");
    std::fs::write(forged.join(".jj").join("repo"), pointer).expect("pointer");
    std::fs::write(
        forged.join(".jj").join("working_copy").join("type"),
        "local",
    )
    .expect("type");
    std::fs::write(
        forged.join(".jj").join("working_copy").join("checkout"),
        root_operation_checkout_record(),
    )
    .expect("checkout record");
    for (tree, session) in [
        (&bare, "session-bare-pointer"),
        (&forged, "session-forged-state"),
    ] {
        std::fs::write(tree.join("AGENTS.md"), "evil instructions").expect("write");
        std::fs::write(tree.join("file.txt"), "content").expect("write");
        let event = post_tool_use_read(repositories.home.path(), &tree.join("file.txt"), session);

        let (success, output, errors) =
            run_hook_input(repositories.home.path(), &event.to_string());

        assert!(success, "{errors}");
        assert!(output.is_empty(), "{}: {output}\n{errors}", tree.display());
        assert!(errors.is_empty(), "{}: {errors}", tree.display());
    }
}

/// A `.jj` store committed inside a git repository, as a directory and as a
/// symlink to one — content a `git clone` delivers verbatim, since git refuses
/// `.git` path components and nothing else. The nearest `.git` above a file in
/// either is the clone's, so the clone's own remotes judge it: a stranger's
/// clone earns nothing; trusting the stranger trusts the clone as the clone,
/// and the store's `upstream` never makes anything managed.
#[test]
fn a_committed_jj_store_or_symlink_is_judged_as_the_clone_that_carries_it() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let attacker = repositories.home.path().join("attacker");
    git_repository(
        &attacker,
        &[("origin", "https://forge.invalid/stranger/clone.git")],
    );
    let evil = attacker.join("evil");
    std::fs::create_dir_all(&evil).expect("evil");
    lab::jj(&evil, ["git", "init", "--no-colocate"]);
    lab::jj(
        &evil,
        [
            "git",
            "remote",
            "add",
            "upstream",
            "https://forge.invalid/maintainer/alpha",
        ],
    );
    lab::jj(
        &evil,
        [
            "git",
            "remote",
            "add",
            "origin",
            "https://forge.invalid/ours/alpha",
        ],
    );
    let _ = std::fs::remove_file(evil.join(".jj").join(".gitignore"));
    std::fs::write(evil.join("AGENTS.md"), "evil instructions").expect("write");
    std::fs::write(evil.join("file.txt"), "content").expect("write");
    let linked = attacker.join("linked");
    std::fs::create_dir_all(&linked).expect("linked");
    std::os::unix::fs::symlink("../evil/.jj", linked.join(".jj")).expect("symlink .jj");
    std::fs::write(linked.join("AGENTS.md"), "linked instructions").expect("write");
    std::fs::write(linked.join("file.txt"), "content").expect("write");
    std::fs::write(attacker.join("AGENTS.md"), "clone instructions").expect("write");
    lab::git_commit_all(&attacker, "attack");
    let clone = repositories.home.path().join("victim-clone");
    lab::git_clone(&attacker, &clone);
    // A clone of a local path points `origin` at the path; a victim's clone of
    // the attacker's forge repository carries the forge URL.
    lab::git_set_remote_url(&clone, "origin", "https://forge.invalid/stranger/clone.git");
    assert!(clone.join("evil").join(".jj").join("repo").is_dir());
    assert!(
        std::fs::symlink_metadata(clone.join("linked").join(".jj"))
            .expect("symlink cloned")
            .file_type()
            .is_symlink()
    );

    // The stranger's clone: unmanaged and untrusted, whatever the trees carry.
    for tree in ["evil", "linked"] {
        let event = post_tool_use_read(
            repositories.home.path(),
            &clone.join(tree).join("file.txt"),
            &format!("session-stranger-{tree}"),
        );
        let (success, output, errors) =
            run_hook_input(repositories.home.path(), &event.to_string());
        assert!(success, "{tree}: {errors}");
        assert!(output.is_empty(), "{tree}: {output}\n{errors}");
        assert!(errors.is_empty(), "{tree}: {errors}");
    }

    // Trusting the stranger trusts the clone, as the clone: the guidance is the
    // clone's (its root file included, nested files as in any trusted repo),
    // and the store's `upstream = alpha` never made anything managed.
    std::fs::write(
        repositories.home.path().join("repos.toml"),
        "[trust]\nowners = [\"stranger\"]\n",
    )
    .expect("registry");
    for tree in ["evil", "linked"] {
        let event = post_tool_use_read(
            repositories.home.path(),
            &clone.join(tree).join("file.txt"),
            &format!("session-trusted-{tree}"),
        );
        let (success, output, errors) =
            run_hook_input(repositories.home.path(), &event.to_string());
        assert!(success, "{tree}: {errors}");
        let context = additional_context(&output);
        assert!(
            context.contains("repo=\"victim-clone\""),
            "{tree}: {context}"
        );
        assert!(context.contains("clone instructions"), "{tree}: {context}");
        assert!(
            !context.contains("fork managed by knives"),
            "{tree}: {context}"
        );
        assert!(errors.is_empty(), "{tree}: {errors}");
    }
}

/// Configuration the environment defines — a global file, `GIT_CONFIG_COUNT`
/// key/value pairs, `GIT_CONFIG_PARAMETERS` as `git -c` exports it — declares
/// a trusted `upstream`; the stranger's clone is still judged by its own
/// configuration file alone.
#[test]
fn configuration_from_the_environment_does_not_lend_a_clone_remotes() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let mine = repositories.home.path().join("mine");
    git_repository(&mine, &[("origin", "https://forge.invalid/stranger/mine")]);
    std::fs::write(mine.join("AGENTS.md"), "mine instructions").expect("write");
    std::fs::write(mine.join("file.txt"), "content").expect("write");
    let trusted = "https://forge.invalid/maintainer/alpha";
    let global = repositories.home.path().join("global.gitconfig");
    std::fs::write(
        &global,
        format!("[remote \"upstream\"]\n\turl = {trusted}\n"),
    )
    .expect("write");
    let injections: [Vec<(&str, String)>; 3] = [
        vec![("GIT_CONFIG_GLOBAL", global.display().to_string())],
        vec![
            ("GIT_CONFIG_COUNT", "1".to_owned()),
            ("GIT_CONFIG_KEY_0", "remote.upstream.url".to_owned()),
            ("GIT_CONFIG_VALUE_0", trusted.to_owned()),
        ],
        vec![(
            "GIT_CONFIG_PARAMETERS",
            format!("'remote.upstream.url={trusted}'"),
        )],
    ];
    for (index, injection) in injections.iter().enumerate() {
        let event = post_tool_use_read(
            repositories.home.path(),
            &mine.join("file.txt"),
            &format!("session-config-env-{index}"),
        );
        let mut command = hook_command(repositories.home.path());
        for (name, value) in injection {
            command.env(name, value);
        }

        let (success, output, errors) = run_command_input(command, &event.to_string());

        assert!(success, "{injection:?}: {errors}");
        assert!(
            output.is_empty(),
            "{injection:?}: a stranger's clone earns nothing: {output}\n{errors}"
        );
        assert!(errors.is_empty(), "{injection:?}: {errors}");
    }
}

/// A colocated checkout's `jj workspace add` workspace carries a `.git` file
/// git resolves to the checkout, so the hook reads the checkout's remotes and
/// answers as for the checkout — with the workspace's own guidance.
#[test]
fn a_workspace_of_a_colocated_checkout_gets_the_checkouts_facts() {
    let repositories = Repositories::new();
    repositories.configure(true);
    Repositories::colocate(&repositories.alpha);
    lab::jj(&repositories.alpha, ["describe", "-m", "init"]);
    lab::jj(&repositories.alpha, ["new"]);
    let workspace = repositories.home.path().join("alpha-feat");
    lab::jj_workspace_add(&repositories.alpha, "feat", &workspace);
    assert!(
        workspace.join(".git").is_file(),
        "a colocated workspace carries a .git file"
    );
    std::fs::write(workspace.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(
        repositories.home.path(),
        &workspace.join("file.txt"),
        "session-colocated-ws",
    );

    let (success, output, errors) = run_hook_input(repositories.home.path(), &event.to_string());

    assert!(success, "{errors}");
    let context = additional_context(&output);
    assert!(context.contains("fork managed by knives"), "{context}");
    assert!(
        context.contains("alpha instructions"),
        "{context}\n{errors}"
    );
    assert!(errors.is_empty(), "{errors}");
}

/// `GIT_DIR` in the hook's environment (git hooks, `rebase -x`, editors export
/// it) names another repository; the touched tree is still judged by its own
/// remotes, not the exporting repository's.
#[test]
fn a_git_dir_in_the_environment_does_not_lend_another_repositorys_remotes() {
    let repositories = Repositories::new();
    repositories.configure(true);
    let other = repositories.home.path().join("other");
    git_repository(&other, &[("origin", "https://forge.invalid/ours/other")]);
    let mine = repositories.home.path().join("mine");
    git_repository(&mine, &[("origin", "https://forge.invalid/stranger/mine")]);
    std::fs::write(mine.join("AGENTS.md"), "mine instructions").expect("write");
    std::fs::write(mine.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(
        repositories.home.path(),
        &mine.join("file.txt"),
        "session-git-dir",
    );
    let mut command = hook_command(repositories.home.path());
    command
        .env("GIT_DIR", other.join(".git"))
        .env("GIT_WORK_TREE", &other);

    let (success, output, errors) = run_command_input(command, &event.to_string());

    assert!(success, "{errors}");
    assert!(
        output.is_empty(),
        "a stranger's clone earns nothing, whatever GIT_DIR says: {output}\n{errors}"
    );
    assert!(errors.is_empty(), "{errors}");
}

/// The same forged pointer inside a genuine git clone: the remotes are read
/// from the clone's own `.git`, so trust and identity are the attacker's, not
/// the pointed-at checkout's.
#[test]
fn a_forged_jj_pointer_inside_a_git_clone_is_judged_by_the_clones_own_remotes() {
    let repositories = Repositories::new();
    repositories.configure(true);
    Repositories::colocate(&repositories.alpha);
    let clone = repositories.home.path().join("clone");
    git_repository(
        &clone,
        &[("origin", "https://forge.invalid/stranger/clone.git")],
    );
    std::fs::create_dir_all(clone.join(".jj")).expect("forged .jj");
    std::fs::write(
        clone.join(".jj").join("repo"),
        repositories
            .alpha
            .join(".jj")
            .join("repo")
            .to_str()
            .expect("utf-8"),
    )
    .expect("forged pointer");
    std::fs::write(clone.join("AGENTS.md"), "clone instructions").expect("write");
    std::fs::write(clone.join("file.txt"), "content").expect("write");
    let event = post_tool_use_read(
        repositories.home.path(),
        &clone.join("file.txt"),
        "session-forged-clone",
    );

    // Unmanaged and untrusted: the stranger's remotes earn nothing.
    let (success, output, errors) = run_hook_input(repositories.home.path(), &event.to_string());
    assert!(success, "{errors}");
    assert!(output.is_empty(), "{output}\n{errors}");

    // Trusting the stranger trusts the clone — through its own remotes.
    std::fs::write(
        repositories.home.path().join("repos.toml"),
        "[trust]\nowners = [\"stranger\"]\n",
    )
    .expect("registry");
    let (success, output, errors) = run_hook_input(repositories.home.path(), &event.to_string());
    assert!(success, "{errors}");
    let context = additional_context(&output);
    assert!(context.contains("clone instructions"), "{context}");
    assert!(!context.contains("fork managed by knives"), "{context}");
}
