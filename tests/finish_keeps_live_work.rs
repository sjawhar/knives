//! `knives finish` never loses work: it snapshots the workspace into jj before
//! forgetting or removing it, and refuses while something may still be working
//! there — another process on this machine inside the workspace, or a claim
//! taken in a place (another host or agent box) whose processes it cannot see.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use lab::{Lab, release_test_home};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};

const OWNER: &str = "ses_fff688";

fn start(lab: &Lab, home: &tempfile::TempDir) -> PathBuf {
    let started = lab::knives_command(
        &lab.work,
        home.path(),
        lab.temp_path(),
        &[
            "--text",
            "start",
            "feat/gamma",
            "--repo",
            "demo",
            "--why",
            "carry the queue fix",
        ],
    )
    .env("KNIVES_OWNER", OWNER)
    .output()
    .expect("start");
    assert!(started.status.success(), "{started:?}");
    let workspace = lab.work.parent().expect("parent").join("feat-gamma");
    assert!(workspace.join(".jj").is_dir(), "fixture: no workspace");
    workspace
}

fn finish(lab: &Lab, home: &tempfile::TempDir, extra: &[&str]) -> Output {
    let mut args = vec!["--text", "finish", "feat/gamma", "--repo", "demo"];
    args.extend_from_slice(extra);
    lab::knives_command(&lab.work, home.path(), lab.temp_path(), &args)
        .env("KNIVES_OWNER", OWNER)
        .output()
        .expect("finish")
}

fn shown(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn claim_held(home: &tempfile::TempDir) -> bool {
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    state["claims"]["demo/feat/gamma"]["owner"] == Value::String(OWNER.to_owned())
}

fn jj(checkout: &Path, args: &[&str]) -> String {
    let output = Command::new("jj")
        .arg("--ignore-working-copy")
        .args(args)
        .current_dir(checkout)
        .output()
        .expect("run jj");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A `sleep` that stops when dropped.
struct Sleeper(Child);

impl Sleeper {
    fn spawn(configure: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new("sleep");
        command.arg("120");
        configure(&mut command);
        Self(command.spawn().expect("spawn sleep"))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn finish_keeps_an_edit_no_jj_command_has_seen_and_names_its_commit() {
    // Given: a file written in the workspace after the last jj command there
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    std::fs::write(workspace.join("notes.txt"), "draft\n").expect("write the edit");

    // When: the branch is finished
    let finished = finish(&lab, &home, &[]);

    // Then: the edit is in a commit of the repository, the one finish names
    assert!(finished.status.success(), "{}", shown(&finished));
    assert!(!workspace.exists(), "{}", shown(&finished));
    let carrying = jj(
        &lab.work,
        &[
            "log",
            "--no-graph",
            "-r",
            r#"files("notes.txt")"#,
            "-T",
            r#"commit_id.short(12) ++ "\n""#,
        ],
    );
    assert!(
        !carrying.trim().is_empty(),
        "no commit holds the edit: {}",
        shown(&finished)
    );
    let stdout = String::from_utf8_lossy(&finished.stdout);
    let commit = stdout
        .split("its working copy is kept as change ")
        .nth(1)
        .and_then(|rest| rest.split("(commit ").nth(1))
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("finish named no commit: {}", shown(&finished)));
    assert_eq!(
        jj(&lab.work, &["file", "show", "-r", commit, "notes.txt"]),
        "draft\n"
    );
}

#[test]
fn finish_changes_nothing_when_jj_cannot_snapshot_every_file() {
    // Given: a new file over jj's default snapshot.max-new-file-size (1 MiB)
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    std::fs::write(workspace.join("big.bin"), vec![b'x'; 2 * 1024 * 1024]).expect("write");

    // When: the branch is finished
    let refused = finish(&lab, &home, &[]);

    // Then: it refuses, the workspace and the file are there, the claim is held
    assert_eq!(refused.status.code(), Some(3), "{}", shown(&refused));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("big.bin"), "{}", shown(&refused));
    assert!(workspace.join("big.bin").is_file());
    assert!(claim_held(&home), "{}", shown(&refused));
    let names = jj(&lab.work, &["workspace", "list", "-T", r#"name ++ "\n""#]);
    assert!(names.contains("feat-gamma"), "{names}");
}

#[test]
fn finish_refuses_while_a_process_works_in_the_workspace() {
    // Given: a process (not knives, not its ancestors, not its session's) with
    // its working directory in the workspace
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let sleeper = Sleeper::spawn(|command| {
        command.current_dir(&workspace);
    });

    // When: the branch is finished
    let refused = finish(&lab, &home, &[]);

    // Then: it refuses and names the process; nothing changed
    assert_eq!(refused.status.code(), Some(2), "{}", shown(&refused));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("pid {} (sleep 120)", sleeper.pid())),
        "{}",
        shown(&refused)
    );
    assert!(workspace.is_dir());
    assert!(claim_held(&home));

    // When: the process is gone
    drop(sleeper);
    let finished = finish(&lab, &home, &[]);

    // Then: finish goes through
    assert!(finished.status.success(), "{}", shown(&finished));
    assert!(!workspace.exists());
}

#[test]
fn finish_refuses_while_a_process_holds_a_file_in_the_workspace_open() {
    // Given: a process elsewhere with a file of the workspace open
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    std::fs::write(workspace.join("notes.txt"), "draft\n").expect("write");
    let held = std::fs::File::open(workspace.join("notes.txt")).expect("open");
    let sleeper = Sleeper::spawn(|command| {
        command.current_dir(lab.temp_path()).stdin(held);
    });

    // When: the branch is finished, then forced
    let refused = finish(&lab, &home, &[]);
    let forced = finish(&lab, &home, &["--force", "--why", "the reader is a test"]);

    // Then: the plain finish names the process; the forced one goes through
    assert_eq!(refused.status.code(), Some(2), "{}", shown(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&format!("pid {}", sleeper.pid())),
        "{}",
        shown(&refused)
    );
    assert!(forced.status.success(), "{}", shown(&forced));
    assert!(!workspace.exists());
}

#[test]
fn the_finishing_sessions_own_processes_do_not_count() {
    // Given: a process of the finishing session (its harness started it, so it
    // carries the session) working in the workspace
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let _own = Sleeper::spawn(|command| {
        command.current_dir(&workspace).env("KNIVES_OWNER", OWNER);
    });

    // When: the branch is finished by that session
    let finished = finish(&lab, &home, &[]);

    // Then: it goes through
    assert!(finished.status.success(), "{}", shown(&finished));
}

#[test]
fn finish_refuses_a_claim_taken_in_another_place() {
    // Given: the claim was taken in another agent box (its place recorded)
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let path = home.path().join("state.json");
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read state"))
            .expect("parse state");
    assert!(
        state["claims"]["demo/feat/gamma"]["place"].is_string(),
        "start recorded no place: {state}"
    );
    state["claims"]["demo/feat/gamma"]["place"] = Value::String("other-box pid:[1]".to_owned());
    std::fs::write(&path, state.to_string()).expect("write state");

    // When: the same session finishes from here, then forces
    let refused = finish(&lab, &home, &[]);
    let forced = finish(&lab, &home, &["--force", "--why", "the box is gone"]);

    // Then: the plain finish refuses and names the other place
    assert_eq!(refused.status.code(), Some(2), "{}", shown(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("other-box pid:[1]"),
        "{}",
        shown(&refused)
    );
    assert!(forced.status.success(), "{}", shown(&forced));
    assert!(!workspace.exists());
}
