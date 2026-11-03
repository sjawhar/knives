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
fn a_duplicate_harness_of_the_same_session_counts() {
    // Given: a process carrying the finishing session that the finishing
    // harness did not start: orphaned away from this test's process tree, as a
    // second harness of the session would be
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let spawned = Command::new("sh")
        .args([
            "-c",
            "cd \"$1\" && nohup sleep 120 >/dev/null 2>&1 & echo $!",
            "sh",
        ])
        .arg(&workspace)
        .env("KNIVES_OWNER", OWNER)
        .output()
        .expect("spawn the duplicate");
    let pid = String::from_utf8_lossy(&spawned.stdout).trim().to_owned();
    assert!(!pid.is_empty(), "{spawned:?}");

    // When: the branch is finished
    let refused = finish(&lab, &home, &[]);
    let _ = Command::new("kill").arg(&pid).status();

    // Then: the duplicate is named and nothing changed
    assert_eq!(refused.status.code(), Some(2), "{}", shown(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&format!("pid {pid} ")),
        "{}",
        shown(&refused)
    );
    assert!(workspace.is_dir());
    assert!(claim_held(&home));
}

#[test]
fn a_duplicate_under_the_same_launcher_counts() {
    // Given: one launcher that carries the session starts a duplicate working
    // in the workspace and, beside it, the harness (`python3`) whose shell runs
    // `knives finish`: knives ← bash ← python3 ← sh, duplicate ← sh
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let script = r#"
        ws=$1; shift
        (cd "$ws" && exec sleep 120) &
        duplicate=$!
        echo "duplicate $duplicate" >&2
        python3 -c 'import subprocess, sys; sys.exit(subprocess.call(sys.argv[1:]))' \
            bash -c '"$@"; exit $?' bash "$@"
        status=$?
        kill "$duplicate"
        exit "$status"
    "#;
    let mut command = Command::new("sh");
    command
        .args(["-c", script, "sh"])
        .arg(&workspace)
        .arg(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "finish", "feat/gamma", "--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_OWNER", OWNER);
    for name in knives::commands::claim::HARNESS_SESSION_VARIABLES {
        if name != "KNIVES_OWNER" {
            command.env_remove(name);
        }
    }
    let refused = command.output().expect("run the launcher");

    // Then: the duplicate is named and nothing changed
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let duplicate = stderr
        .lines()
        .find_map(|line| line.strip_prefix("duplicate "))
        .unwrap_or_else(|| panic!("no duplicate started: {}", shown(&refused)))
        .to_owned();
    assert_eq!(refused.status.code(), Some(2), "{}", shown(&refused));
    assert!(
        stderr.contains(&format!("pid {duplicate} ")),
        "{}",
        shown(&refused)
    );
    assert!(workspace.is_dir());
    assert!(claim_held(&home));
}

/// Moves the workspace at `workspace` to a sibling and leaves `workspace` a
/// chain of two symbolic links to it; returns where it now is.
fn behind_two_links(workspace: &Path) -> PathBuf {
    let real = workspace.with_file_name("feat-gamma-real");
    let middle = workspace.with_file_name("feat-gamma-middle");
    std::fs::rename(workspace, &real).expect("move the workspace");
    std::os::unix::fs::symlink(&real, &middle).expect("link the middle");
    std::os::unix::fs::symlink(&middle, workspace).expect("link the path");
    real
}

#[test]
fn finish_keeps_the_edit_of_a_workspace_behind_a_chain_of_links() {
    // Given: the workspace, with an edit, reached through two symbolic links
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    std::fs::write(workspace.join("notes.txt"), "draft\n").expect("write the edit");
    let real = behind_two_links(&workspace);

    // When: the branch is finished
    let finished = finish(&lab, &home, &[]);

    // Then: the edit is in a commit, and the links and the directory stay
    assert!(finished.status.success(), "{}", shown(&finished));
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
    assert!(!carrying.trim().is_empty(), "{}", shown(&finished));
    assert!(real.join("notes.txt").is_file());
}

#[test]
fn finish_does_not_forget_an_unreadable_workspace_behind_a_link() {
    // Given: the workspace reached through a symbolic link, its state corrupt
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let real = behind_two_links(&workspace);
    std::fs::write(real.join(".jj/working_copy/checkout"), b"\xff\xfe garbage")
        .expect("corrupt the working-copy state");

    // When: the branch is finished
    let refused = finish(&lab, &home, &[]);

    // Then: it refuses; the workspace stays registered and the claim held
    assert_eq!(refused.status.code(), Some(3), "{}", shown(&refused));
    let names = jj(&lab.work, &["workspace", "list", "-T", r#"name ++ "\n""#]);
    assert!(names.contains("feat-gamma"), "{names}");
    assert!(claim_held(&home));
}

#[test]
fn finish_changes_nothing_when_a_file_name_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt as _;
    // Given: a file whose name jj cannot snapshot
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    let name = std::ffi::OsStr::from_bytes(b"draft-\xff.txt");
    std::fs::write(workspace.join(name), "draft\n").expect("write");

    // When: the branch is finished
    let refused = finish(&lab, &home, &[]);

    // Then: it refuses; the file, the workspace and the claim stay
    assert_eq!(refused.status.code(), Some(3), "{}", shown(&refused));
    assert!(workspace.join(name).is_file());
    assert!(claim_held(&home));
}

#[test]
fn finish_does_not_forget_a_workspace_whose_state_cannot_be_read() {
    // Given: the workspace's working-copy state is corrupt
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    std::fs::write(
        workspace.join(".jj/working_copy/checkout"),
        b"\xff\xfe garbage",
    )
    .expect("corrupt the working-copy state");

    // When: the branch is finished
    let refused = finish(&lab, &home, &[]);

    // Then: it refuses; the workspace stays registered and the claim held
    assert_eq!(refused.status.code(), Some(3), "{}", shown(&refused));
    let names = jj(&lab.work, &["workspace", "list", "-T", r#"name ++ "\n""#]);
    assert!(names.contains("feat-gamma"), "{names}");
    assert!(claim_held(&home));
}

#[test]
fn the_kept_commit_is_named_even_when_removal_fails() {
    use std::os::unix::fs::PermissionsExt as _;
    // Given: an edit, and a directory removal cannot get through
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let workspace = start(&lab, &home);
    std::fs::write(workspace.join("notes.txt"), "draft\n").expect("write the edit");
    let stuck = workspace.join("stuck");
    std::fs::create_dir(&stuck).expect("create");
    std::fs::write(stuck.join("inside.txt"), "x\n").expect("write");
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o555)).expect("chmod");

    // When: the branch is finished
    let finished = finish(&lab, &home, &[]);
    std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // Then: whatever the removal did, the output names the commit that keeps
    // the edit
    assert!(
        String::from_utf8_lossy(&finished.stdout).contains("its working copy is kept as change "),
        "{}",
        shown(&finished)
    );
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

    // When: the same session finishes from here
    let refused = finish(&lab, &home, &[]);

    // Then: it refuses, names the other place, and keeps the claim and the
    // workspace
    assert_eq!(refused.status.code(), Some(2), "{}", shown(&refused));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("other-box pid:[1]"),
        "{}",
        shown(&refused)
    );
    assert!(claim_held(&home));
    assert!(workspace.is_dir());

    // When: it forces, then the finish goes through
    let forced = finish(&lab, &home, &["--force", "--why", "the box is gone"]);
    assert!(forced.status.success(), "{}", shown(&forced));
    assert!(!workspace.exists());
}
