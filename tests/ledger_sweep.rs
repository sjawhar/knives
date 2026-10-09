#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! `knives ledger sweep` and the hand-off every write makes to it, through the
//! real binary: a config home whose ledger root is a git repository sharing a
//! bare remote, as a machine set up to share its ledger has.

#[path = "common/lab.rs"]
mod lab;

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use knives::ledger::{Entry, Kind, Ledger};

/// Make `home`'s ledger root a repository that shares `forks` with `remote`
/// as `machine`, and return the root.
fn share(home: &Path, machine: &str, remote: &Path, forks: &[&str]) -> PathBuf {
    let root = home.join("ledger");
    lab::git_repository(&root, &[("origin", remote.to_str().expect("utf-8"))]);
    lab::git_output(&root, ["config", "knives.machine", machine]);
    for fork in forks {
        lab::git_output(&root, ["config", "--add", "knives.fork", fork]);
    }
    root
}

/// A bare remote under a fresh directory.
fn bare_remote() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("remote directory");
    let remote = root.path().join("ledger.git");
    lab::git_bare_repository(&remote);
    (root, remote)
}

fn append(root: &Path, fork: &str, count: usize) {
    let ledger = Ledger::at(root.join(fork));
    for index in 0..count {
        ledger
            .append(&Entry {
                ts: format!("2026-10-09T12:00:00.{index:09}Z"),
                owner: "ses_sweep".to_owned(),
                subject: None,
                kind: Kind::Note,
                disposition: None,
                statement: None,
                text: format!("entry {index}"),
                evidence: Vec::new(),
                anchor: None,
                pr: None,
                parents: Vec::new(),
            })
            .expect("append an entry");
    }
}

fn sweep(home: &Path) -> Output {
    lab::knives_command(home, home, home, &["--json", "ledger", "sweep"])
        .output()
        .expect("run knives ledger sweep")
}

/// `git --git-dir=<git_dir> <args>`'s trimmed stdout, or `None` when it fails.
fn git_in(git_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .expect("run git");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// How many entry files `machine`'s ref holds on `remote`; `0` before it has one.
fn entries_on(remote: &Path, machine: &str) -> usize {
    git_in(
        remote,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            &format!("refs/knives/{machine}"),
        ],
    )
    .map_or(0, |listing| {
        listing.lines().filter(|path| is_entry(path)).count()
    })
}

/// Whether `path` names a ledger entry file.
fn is_entry(path: &str) -> bool {
    Path::new(path).extension() == Some(std::ffi::OsStr::new("md"))
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether some process holds the lock file at `path`.
fn held(path: &Path) -> bool {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .expect("open a lock file");
    match file.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => panic!("locking {}: {error}", path.display()),
    }
}

/// Wait until no sweep holds `home`'s ledger lock, so none outlives the test.
fn settle(home: &Path) {
    wait_until("the last sweep to finish", || {
        !held(&home.join("ledger.lock"))
    });
}

#[test]
fn a_burst_of_concurrent_sweeps_makes_one_commit_and_no_error() {
    // Given: twelve entries on a machine that shares its ledger.
    const SWEEPS: usize = 16;
    let home = tempfile::tempdir().expect("config home");
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote, &["a-repo"]);
    append(&root, "a-repo", 12);

    // When: sixteen sweeps start at once.
    let children: Vec<_> = (0..SWEEPS)
        .map(|_| {
            lab::knives_command(
                home.path(),
                home.path(),
                home.path(),
                &["--json", "ledger", "sweep"],
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("start a sweep")
        })
        .collect();
    // Every sweep is started before any is waited on, so they overlap.
    let mut outputs: Vec<Output> = Vec::with_capacity(SWEEPS);
    for child in children {
        outputs.push(child.wait_with_output().expect("wait for a sweep"));
    }

    // Then: every sweep succeeded and said nothing on stderr, the machine's
    // ref holds exactly one commit carrying all twelve, and it reached the
    // remote; no sweep left a failure behind.
    for output in &outputs {
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "a sweep failed: {:?}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let git_dir = root.join(".git");
    assert_eq!(
        git_in(&git_dir, &["rev-list", "--count", "refs/knives/alpha"]).as_deref(),
        Some("1")
    );
    assert_eq!(entries_on(&remote, "alpha"), 12);
    assert!(!home.path().join("ledger-sweep.log").exists());
    // And: at least one sweep found the lock taken and left, rather than wait.
    let outcomes: Vec<String> = outputs
        .iter()
        .map(|output| {
            let report: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("a sweep reports JSON");
            report["outcome"].as_str().expect("an outcome").to_owned()
        })
        .collect();
    assert!(
        outcomes.iter().any(|outcome| outcome == "busy"),
        "no sweep ever found another running: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome == "busy" || outcome == "swept"),
        "{outcomes:?}"
    );
}

#[test]
fn a_sweep_with_nothing_new_does_nothing_and_succeeds() {
    // Given: a machine whose one entry is already swept.
    let home = tempfile::tempdir().expect("config home");
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote, &["a-repo"]);
    append(&root, "a-repo", 1);
    assert!(sweep(home.path()).status.success());
    let git_dir = root.join(".git");
    let before = git_in(&git_dir, &["rev-parse", "refs/knives/alpha"]).expect("a ref");

    // When: it sweeps again.
    let again = sweep(home.path());

    // Then: it succeeded, committed and pushed nothing, and moved no ref.
    assert!(again.status.success(), "{again:?}");
    let report: serde_json::Value = serde_json::from_slice(&again.stdout).expect("JSON");
    assert_eq!(report["outcome"], "swept", "{report}");
    assert_eq!(report["destinations"][0]["commits"], 0, "{report}");
    assert_eq!(report["destinations"][0]["pushes"], 0, "{report}");
    assert_eq!(
        git_in(&git_dir, &["rev-parse", "refs/knives/alpha"]),
        Some(before.clone())
    );
    assert_eq!(
        git_in(&remote, &["rev-parse", "refs/knives/alpha"]),
        Some(before)
    );
}

#[test]
fn a_ledger_nobody_set_up_to_share_is_swept_as_nothing_and_touched_nowhere() {
    // Given: a config home whose ledger root is no repository at all, and
    // another whose repository names no machine or fork.
    for initialised in [false, true] {
        let home = tempfile::tempdir().expect("config home");
        let root = home.path().join("ledger");
        if initialised {
            lab::git_repository(&root, &[]);
        }
        append(&root, "a-repo", 1);

        // When: a sweep runs.
        let swept = sweep(home.path());

        // Then: it succeeds as not shared and leaves no lock, log or ref.
        assert!(swept.status.success(), "{swept:?}");
        let report: serde_json::Value = serde_json::from_slice(&swept.stdout).expect("JSON");
        assert_eq!(report["outcome"], "not-shared", "{report}");
        assert!(!home.path().join("ledger.lock").exists());
        assert!(!home.path().join("ledger-sweep.log").exists());
        if initialised {
            assert_eq!(
                git_in(&root.join(".git"), &["for-each-ref", "refs/knives/"]).as_deref(),
                Some("")
            );
        }
    }
}

#[test]
fn a_destination_with_forks_but_no_machine_name_fails_naming_the_fix() {
    // Given: a ledger repository that carries a fork and names no machine.
    let home = tempfile::tempdir().expect("config home");
    let root = home.path().join("ledger");
    lab::git_repository(&root, &[]);
    lab::git_output(&root, ["config", "knives.fork", "a-repo"]);

    // When: a sweep runs.
    let swept = sweep(home.path());

    // Then: it fails, names the command that fixes it, and leaves that in the log.
    assert_eq!(swept.status.code(), Some(3), "{swept:?}");
    let fix = format!(
        "git --git-dir={} config knives.machine <name>",
        root.join(".git").display()
    );
    assert!(
        String::from_utf8_lossy(&swept.stdout).contains(&fix),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    let log = std::fs::read_to_string(home.path().join("ledger-sweep.log")).expect("a log");
    assert!(log.contains(&fix), "{log}");
}

#[test]
fn a_write_hands_off_to_a_sweep_and_does_not_wait_for_it() {
    // Given: a managed fork on a machine that shares its ledger, with the
    // ledger repository's transport lock held, so any sweep stalls in its
    // first pass until the test lets go.
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote, &["demo"]);
    let transport = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".git").join("knives-transport.lock"))
        .expect("open the transport lock");
    transport.lock().expect("hold the transport lock");

    // When: a note is written.
    let started = Instant::now();
    let wrote = lab::knives_command(
        &lab.work,
        home.path(),
        lab.temp_path(),
        &["--text", "notch", "-m", "handed off", "--repo", "demo"],
    )
    .env("KNIVES_OWNER", "ses_fff688")
    .output()
    .expect("run knives notch");
    let took = started.elapsed();

    // Then: the write returned while its sweep was still stalled, holding
    // the ledger's lock: it did not wait for the sweep.
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    assert!(took < Duration::from_secs(30), "the write took {took:?}");
    wait_until("the handed-off sweep to take the ledger lock", || {
        held(&home.path().join("ledger.lock"))
    });
    assert_eq!(entries_on(&remote, "alpha"), 0);

    // When: the transport lock is let go.
    drop(transport);

    // Then: the sweep carries the note to the remote.
    wait_until("the note to reach the remote", || {
        entries_on(&remote, "alpha") == 1
    });
    settle(home.path());
    assert!(!home.path().join("ledger-sweep.log").exists());
}

#[test]
fn a_burst_of_concurrent_writes_reaches_the_remote_without_a_failure() {
    // Given: a managed fork on a machine that shares its ledger.
    const WRITES: usize = 16;
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    share(home.path(), "alpha", &remote, &["demo"]);

    // When: sixteen notes are written at once, each handing off.
    let children: Vec<_> = (0..WRITES)
        .map(|index| {
            lab::knives_command(
                &lab.work,
                home.path(),
                lab.temp_path(),
                &[
                    "--text",
                    "notch",
                    "-m",
                    &format!("note {index}"),
                    "--repo",
                    "demo",
                ],
            )
            .env("KNIVES_OWNER", "ses_fff688")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("start a write")
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().expect("wait for a write");
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "a write failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Then: every note reaches the remote, in fewer commits than writes, and
    // no sweep left a failure behind.
    wait_until("every note to reach the remote", || {
        entries_on(&remote, "alpha") == WRITES
    });
    settle(home.path());
    assert!(!home.path().join("ledger-sweep.log").exists());
    let commits: usize = git_in(&remote, &["rev-list", "--count", "refs/knives/alpha"])
        .expect("a ref")
        .parse()
        .expect("a count");
    assert!(commits < WRITES, "{commits} commits for {WRITES} writes");
}
