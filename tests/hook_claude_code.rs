#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unreachable,
    reason = "fixture setup failures and JSON shape mismatches are test failures"
)]
// allow: SIZE_OK: 437 lines - real-binary adapter scenarios share one fixture and process harness.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

fn run_hook_input(home: &Path, input: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["hook", "claude-code"])
        .env("KNIVES_CONFIG_HOME", home)
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

fn run_hook(home: &Path, event: &Value) -> String {
    let (success, output, errors) = run_hook_input(home, &event.to_string());
    assert!(success, "a hook must never fail the session: {errors}");
    output
}

struct Repositories {
    home: tempfile::TempDir,
    alpha: PathBuf,
    beta: PathBuf,
    trusted: PathBuf,
}

impl Repositories {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("config home");
        let alpha = home.path().join("alpha");
        let beta = home.path().join("beta");
        let trusted = home.path().join("trusted");
        for (root, instructions) in [
            (&alpha, "alpha instructions"),
            (&beta, "beta instructions"),
            (&trusted, "trusted instructions"),
        ] {
            std::fs::create_dir_all(root).expect("create repository");
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
        let trusted = include_trusted.then(|| {
            format!(
                "\n[trusted.trusted]\npath = \"{}\"\n",
                self.trusted.display()
            )
        });
        let config = format!(
            "[repos.alpha]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [repos.beta]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n{}",
            self.alpha.display(),
            self.beta.display(),
            trusted.unwrap_or_default(),
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
    std::fs::create_dir_all(repos.beta.join(".jj")).expect("create workspace marker");
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
    std::fs::create_dir_all(repos.beta.join(".jj")).expect("create workspace marker");
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
    // Given: a session in alpha that reads a file in beta.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );

    // When: the same foreign file is read twice.
    let first = run_hook(repos.home.path(), &read);
    let second = run_hook(repos.home.path(), &read);

    // Then: only the first read carries beta's notice and guidance.
    let context = additional_context(&first);
    assert_eq!(
        response_event_name(&first),
        read["hook_event_name"].as_str().expect("event name")
    );
    assert!(context.contains("fork managed by knives"), "was: {context}");
    assert!(context.contains("beta instructions"), "was: {context}");
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
    std::fs::create_dir_all(repos.alpha.join(".jj")).expect("create alpha workspace marker");
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
fn post_tool_use_without_cwd_emits_notice_without_guidance() {
    // Given: a managed file in a PostToolUse event that has no session cwd.
    let repos = Repositories::new();
    repos.configure(false);
    let mut read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );
    read.as_object_mut().expect("event object").remove("cwd");

    // When: the foreign file is read.
    let context = additional_context(&run_hook(repos.home.path(), &read));

    // Then: the managed-repository notice remains but repository guidance is omitted.
    assert!(context.contains("fork managed by knives"), "was: {context}");
    assert!(!context.contains("beta instructions"), "was: {context}");
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
    // Given: a foreign repository was announced once.
    let repos = Repositories::new();
    repos.configure(false);
    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );
    let compact = event("pre-compact", &repos.alpha, None);
    let first = run_hook(repos.home.path(), &read);

    // When: Claude Code compacts its context and reads that repository again.
    assert!(run_hook(repos.home.path(), &compact).is_empty());
    let second = run_hook(repos.home.path(), &read);

    // Then: both reads receive the full foreign guidance.
    assert!(additional_context(&first).contains("beta instructions"));
    assert!(additional_context(&second).contains("beta instructions"));
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
    // Given: a managed repository initially has no instruction file.
    let repos = Repositories::new();
    repos.configure(false);
    std::fs::remove_file(repos.beta.join("AGENTS.md")).expect("remove instructions");
    let read = event(
        "post-tool-read",
        &repos.alpha,
        Some(&repos.beta.join("file.txt")),
    );

    // When: the notice emitted on the first read, then instructions appear.
    let first = run_hook(repos.home.path(), &read);
    assert!(additional_context(&first).contains("fork managed by knives"));
    std::fs::write(repos.beta.join("AGENTS.md"), "later instructions").expect("write instructions");
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
