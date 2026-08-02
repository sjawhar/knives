#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "fixture setup failures and JSON shape mismatches are test failures"
)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

const SESSION_ID: &str = "opencode-hook-test-session";

fn run_hook_input(home: &Path, input: &str) -> (bool, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["hook", "opencode"])
        .env("KNIVES_CONFIG_HOME", home)
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
    let (success, output, errors) = run_hook_input(home, &event.to_string());
    assert!(success, "a hook must never fail the session: {errors}");
    serde_json::from_str(&output).expect("hook output JSON")
}

fn addition(output: &Value) -> &str {
    output["addition"].as_str().expect("tool addition")
}

struct Repositories {
    home: tempfile::TempDir,
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
        let config = format!(
            "[repos.alpha]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [repos.beta]\npath = \"{}\"\nupstream = \"u\"\norigin = \"o\"\n\n\
             [trusted.trusted]\npath = \"{}\"\n",
            alpha.display(),
            beta.display(),
            trusted.display()
        );
        std::fs::write(home.path().join("repos.toml"), config).expect("write registry");
        let state = json!({"claims": {"beta/feat/claimed": {
            "repo": "beta", "branch": "feat/claimed", "owner": "agent-one",
            "why": "porting", "started": "2026-01-01T00:00:00Z", "files": []
        }}});
        std::fs::write(home.path().join("state.json"), state.to_string()).expect("write state");
        Self {
            home,
            beta,
            trusted,
        }
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
fn tool_after_honors_disabled_notice_and_trusted_roots() {
    // Given: managed and trusted repositories with instructions.
    let repos = Repositories::new();

    // When: managed guidance disables its notice and trusted guidance permits all parts.
    let managed = run_hook(
        repos.home.path(),
        &tool(
            &repos.beta.join("file.txt"),
            Some(json!({"notice": false, "guidance": true})),
        ),
    );
    let trusted = run_hook(
        repos.home.path(),
        &tool(&repos.trusted.join("file.txt"), None),
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
fn shell_env_returns_the_managed_claim_owner_only() {
    // Given: a managed root with one claim, a trusted root, and an outside directory.
    let repos = Repositories::new();
    let outside = repos.home.path().join("outside");
    std::fs::create_dir_all(&outside).expect("create outside directory");

    // When: the shim requests an owner for each directory.
    let managed = run_hook(
        repos.home.path(),
        &json!({"event": "shell.env", "cwd": repos.beta}),
    );
    let trusted = run_hook(
        repos.home.path(),
        &json!({"event": "shell.env", "cwd": repos.trusted}),
    );
    let outside = run_hook(
        repos.home.path(),
        &json!({"event": "shell.env", "cwd": outside}),
    );

    // Then: only the managed repository has an owner.
    assert_eq!(managed, json!({"owner": "agent-one"}));
    assert_eq!(trusted, json!({"owner": null}));
    assert_eq!(outside, json!({"owner": null}));
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
}

#[test]
fn malformed_and_pathless_input_return_empty_responses_without_failing() {
    // Given: malformed input and a pathless Bash event with an invalid registry.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("repos.toml"), "[[[garbage").expect("write malformed registry");

    // When: each reaches the hook binary.
    let (success, malformed, errors) = run_hook_input(home.path(), "not json");
    let pathless = run_hook(
        home.path(),
        &json!({"event": "tool.execute.after", "session_id": SESSION_ID, "tool": "bash", "args": {}}),
    );

    // Then: neither can interrupt OpenCode, and the pathless fast path skips the registry.
    assert!(success);
    assert_eq!(
        serde_json::from_str::<Value>(&malformed).expect("empty JSON response"),
        json!({})
    );
    assert!(!errors.is_empty(), "malformed input is reported on stderr");
    assert_eq!(pathless, json!({"addition": ""}));
}
