#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "fixture setup failures and JSON shape mismatches are test failures"
)]

#[path = "common/lab.rs"]
mod lab;

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
        .env("JJ_CONFIG", "/dev/null")
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
        "repo": "beta",
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
        "beta/feat/claimed": claim("feat/claimed"),
        "beta/feat/new": claim("feat/new")
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
        seen["workspaces"]["beta/beta"]
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
        "beta/feat/one": {
            "repo": "beta", "branch": "feat/one", "owner": "agent-one",
            "why": "one", "started": "2026-01-01T00:00:00Z", "files": []
        },
        "beta/feat/two": {
            "repo": "beta", "branch": "feat/two", "owner": "agent-two",
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
        .env("JJ_CONFIG", "/dev/null")
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
    // 2026-08-25: without a watchdog the process parks in its stdin read.
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
