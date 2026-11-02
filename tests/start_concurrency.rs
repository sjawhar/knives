//! Concurrent `knives start`s: what waits for what.
//!
//! A `start` holds its fork's lock for the whole claim, fetch and workspace
//! creation, and the state file's lock only for the write that records the
//! claim. So a `start` on another fork does not wait for it, while a `start`
//! on the same fork waits its turn and then decides from what the first one
//! wrote.
//!
//! The first `start` is held inside its fetch by a `git` on its `PATH` that
//! waits for a gate file, so each test sees the first `start` mid-claim for
//! as long as it needs, whatever the machine's speed.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lab::Lab;
use serde_json::Value;

/// Two forks, `demo` and `other`, whose checkouts one scan root holds, and the
/// registry naming both.
struct Estate {
    root: tempfile::TempDir,
    home: tempfile::TempDir,
    demo: Lab,
    other: Lab,
    /// The `git` that waits for [`Estate::gate`] before a fetch.
    gated_path: std::ffi::OsString,
}

impl Estate {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("scan root");
        let demo = Lab::new_in(root.path());
        let other = Lab::new_in(root.path());
        let home = tempfile::tempdir().expect("config home");
        std::fs::write(
            home.path().join("repos.toml"),
            format!(
                "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/demo.git\"\n\n\
                 [repos.other]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/other.git\"\n",
                demo.upstream.display(),
                other.upstream.display(),
            ),
        )
        .expect("write registry");
        let gated_path = gated_git(root.path());
        Self {
            root,
            home,
            demo,
            other,
            gated_path,
        }
    }

    /// The file whose existence lets a gated fetch go on.
    fn gate(&self) -> PathBuf {
        self.root.path().join("gate")
    }

    /// The file a gated fetch writes when it starts waiting.
    fn waiting(&self) -> PathBuf {
        self.root.path().join("gate.waiting")
    }

    /// `knives start <branch> --repo <repo>` as `owner`, from that fork's
    /// checkout, not yet run.
    fn start(&self, repo: &str, branch: &str, owner: &str) -> Command {
        let lab = if repo == "demo" {
            &self.demo
        } else {
            &self.other
        };
        let mut command = lab::knives_command(
            &lab.work,
            self.home.path(),
            self.root.path(),
            &["--text", "start", branch, "--repo", repo, "--why", "test"],
        );
        command
            .env("KNIVES_OWNER", owner)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// [`Estate::start`], with its fetch held at the gate; returns once the
    /// fetch is waiting there, so the `start` holds whatever it holds across
    /// its fetch.
    fn start_held(&self, repo: &str, branch: &str, owner: &str) -> Child {
        let child = self
            .start(repo, branch, owner)
            .env("PATH", &self.gated_path)
            .env("LAB_FETCH_GATE", self.gate())
            .spawn()
            .expect("spawn the held start");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self.waiting().exists() {
            assert!(
                Instant::now() < deadline,
                "the held start never reached its fetch"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        child
    }

    fn open_gate(&self) {
        std::fs::write(self.gate(), "").expect("open the gate");
    }

    /// Every claim in the state file, as `(repo/branch, owner)`.
    fn claims(&self) -> Vec<(String, String)> {
        let text =
            std::fs::read_to_string(self.home.path().join("state.json")).expect("state file");
        let state: Value = serde_json::from_str(&text).expect("state JSON");
        state["claims"]
            .as_object()
            .expect("claims")
            .iter()
            .map(|(key, claim)| {
                (
                    key.clone(),
                    claim["owner"].as_str().expect("owner").to_owned(),
                )
            })
            .collect()
    }
}

/// A directory holding a `git` that, when `LAB_FETCH_GATE` is set, writes
/// `$LAB_FETCH_GATE.waiting` and waits (up to a minute) for `$LAB_FETCH_GATE`
/// to exist before a fetch, then runs the real `git`; and the `PATH` with it
/// first. jj fetches through the `git` on its `PATH`.
fn gated_git(root: &Path) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").expect("PATH");
    let real = std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    let directory = root.join("gated-bin");
    std::fs::create_dir_all(&directory).expect("gated bin directory");
    let script = directory.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             case \" $* \" in\n\
             *\" fetch \"*)\n\
             \x20 if [ -n \"$LAB_FETCH_GATE\" ]; then\n\
             \x20   : > \"$LAB_FETCH_GATE.waiting\"\n\
             \x20   i=0\n\
             \x20   while [ ! -e \"$LAB_FETCH_GATE\" ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done\n\
             \x20 fi;;\n\
             esac\n\
             exec '{}' \"$@\"\n",
            real.display()
        ),
    )
    .expect("write gated git");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("chmod gated git");
    std::env::join_paths(std::iter::once(directory).chain(std::env::split_paths(&path)))
        .expect("join PATH")
}

/// Whether `child` exits within `limit`.
fn exits_within(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait().expect("poll child").is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finished(child: Child, what: &str) -> std::process::Output {
    let output = child.wait_with_output().expect("collect output");
    assert!(
        output.status.code().is_some(),
        "{what} was killed: {output:?}"
    );
    output
}

#[test]
fn a_start_on_another_fork_does_not_wait_for_one_mid_claim() {
    // Given: a start on `demo` held inside its fetch, mid-claim.
    let estate = Estate::new();
    let mut held = estate.start_held("demo", "feat/alpha", "agent-one");

    // When: a start on `other` runs.
    let mut other = estate
        .start("other", "feat/beta", "agent-two")
        .spawn()
        .expect("spawn the other start");

    // Then: it finishes while the first is still held; one minute is the
    // claim-lock budget, so a start that waited for the first would not.
    let done = exits_within(&mut other, Duration::from_secs(30));
    let still_held = held.try_wait().expect("poll held").is_none();
    estate.open_gate();
    let held = finished(held, "the held start");
    assert!(done, "the start on another fork waited for the held one");
    assert!(still_held, "the held start was not held: {held:?}");
    let other = finished(other, "the other start");
    assert!(other.status.success(), "{other:?}");
    assert!(held.status.success(), "{held:?}");
    let mut claims = estate.claims();
    claims.sort();
    assert_eq!(
        claims,
        [
            ("demo/feat/alpha".to_owned(), "agent-one".to_owned()),
            ("other/feat/beta".to_owned(), "agent-two".to_owned()),
        ],
        "a claim was lost"
    );
}

#[test]
fn a_start_on_the_same_fork_waits_for_one_mid_claim() {
    // Given: a start on `demo` held inside its fetch, mid-claim.
    let estate = Estate::new();
    let held = estate.start_held("demo", "feat/alpha", "agent-one");

    // When: a start of another branch on `demo` runs.
    let mut second = estate
        .start("demo", "feat/beta", "agent-two")
        .spawn()
        .expect("spawn the second start");

    // Then: it does not finish while the first holds the fork, and does once
    // the first is done, with both claims kept.
    let early = exits_within(&mut second, Duration::from_secs(3));
    estate.open_gate();
    let held = finished(held, "the held start");
    assert!(!early, "the second start on one fork did not wait");
    assert!(held.status.success(), "{held:?}");
    let second = finished(second, "the second start");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(estate.claims().len(), 2, "{:?}", estate.claims());
}

#[test]
fn two_starts_of_one_branch_leave_one_claim() {
    // Given: agent-one's start of `feat/alpha` held mid-claim.
    let estate = Estate::new();
    let held = estate.start_held("demo", "feat/alpha", "agent-one");

    // When: agent-two starts the same branch.
    let mut second = estate
        .start("demo", "feat/alpha", "agent-two")
        .spawn()
        .expect("spawn the second start");
    let early = exits_within(&mut second, Duration::from_secs(3));
    estate.open_gate();
    let held = finished(held, "the held start");
    let second = finished(second, "the second start");

    // Then: it waited for the first, then refused on agent-one's claim.
    assert!(!early, "the second start of one branch did not wait");
    assert!(held.status.success(), "{held:?}");
    assert_eq!(second.status.code(), Some(2), "{second:?}");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert_eq!(
        estate.claims(),
        [("demo/feat/alpha".to_owned(), "agent-one".to_owned())]
    );
}
