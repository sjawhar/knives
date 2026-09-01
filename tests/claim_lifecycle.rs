#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    dead_code,
    reason = "the shared lab fixture exposes helpers each isolated test target does not use"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::jj::Repo;
use lab::{operation_ids, release_test_home};
use serde_json::Value;
use std::process::Command;
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
            .env_remove("KNIVES_OWNER")
            .env_remove("CLAUDE_CODE_SESSION_ID")
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
    assert!(
        stderr.contains("both sides are anonymous identities"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("can never match"), "stderr: {stderr}");
    assert!(stderr.contains("port it"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
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
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert!(stderr.contains("harness-session"), "stderr: {stderr}");
    assert!(stderr.contains("claimed"), "stderr: {stderr}");
    assert!(stderr.contains("last seen"), "stderr: {stderr}");
    assert!(stderr.contains("--force"), "stderr: {stderr}");
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
        .env_remove("KNIVES_OWNER")
        .env_remove("CLAUDE_CODE_SESSION_ID")
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
            "--allow-open",
            "--no-cleanup",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
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
    // Given: a held claim and a ledger path deliberately made unwritable as a directory.
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let run = |owner: &str, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
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
    let ledger = home.path().join("ledger");
    std::fs::rename(&ledger, home.path().join("ledger-backup")).expect("move ledger aside");
    std::fs::write(&ledger, "not a directory").expect("block ledger append");

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

    // Then: the forced claim never becomes current without its event.
    assert!(
        !forced.status.success(),
        "force unexpectedly succeeded: {forced:?}"
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
        .env("KNIVES_OWNER", "agent-one")
        .output()
        .expect("start claim");
    assert!(start.status.success(), "{start:?}");
    let state_before = std::fs::read(home.path().join("state.json")).expect("read state");
    let ledger = home.path().join("ledger");
    std::fs::rename(&ledger, home.path().join("ledger-backup")).expect("move ledger aside");
    std::fs::write(&ledger, "not a directory").expect("block ledger append");

    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/gamma",
            "--repo",
            "demo",
            "--allow-open",
            "--no-cleanup",
            "--force",
            "--why",
            "release stalled work",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "agent-two")
        .output()
        .expect("force finish");

    assert!(
        !finished.status.success(),
        "finish unexpectedly succeeded: {finished:?}"
    );
    assert_eq!(
        std::fs::read(home.path().join("state.json")).expect("read state"),
        state_before,
        "ledger failure saved an unprovenanced forced finish"
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
        .output()
        .expect("parse start");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--why"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
