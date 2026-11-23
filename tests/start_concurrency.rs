//! Concurrent `knives start`s, and a `sync` beside another writer of its
//! fork: what waits for what.
//!
//! A `start` holds its branch's lock for the whole claim, fetch and workspace
//! creation, and the state file's lock only for the write that records the
//! claim. So a `start` of another branch, of this fork or another, does not
//! wait for it, while a `start` of the same branch waits its turn and then
//! decides from what the first one wrote. A `sync` takes its fork's lock only
//! once it has fetched, so a writer of the fork does not wait for its fetch.
//!
//! The first command is held inside its fetch by a `git` on its `PATH` that
//! waits for a gate file, so each test sees it mid-fetch for as long as it
//! needs, whatever the machine's speed. Where a test needs to know a command
//! waits for a lock, it reads that the command holds a lock file open
//! ([`lab::waits_for_a_lock`]), not how long it took.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

#[path = "common/forge_shim.rs"]
mod forge_shim;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lab::{Holding, Lab};
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
/// `$LAB_FETCH_GATE.waiting` and waits (up to four minutes) for
/// `$LAB_FETCH_GATE` to exist before a fetch, then runs the real `git`; and
/// the `PATH` with it first. jj fetches through the `git` on its `PATH`. Four
/// minutes is past the minute a waiter gives a lock, so a command that waits
/// for the held one's lock gives up while the held one is still held.
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
             \x20   while [ ! -e \"$LAB_FETCH_GATE\" ] && [ $i -lt 4800 ]; do sleep 0.05; i=$((i+1)); done\n\
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

    // When: a start on `other` runs to its end. Had it waited for the held
    // start, it would have given up the wait after a minute, while the held
    // start is still held, and exited 3.
    let other = estate
        .start("other", "feat/beta", "agent-two")
        .output()
        .expect("run the other start");
    let still_held = held.try_wait().expect("poll held").is_none();
    estate.open_gate();
    let held = finished(held, "the held start");

    // Then: it took its claim while the first was held, and both claims are
    // kept.
    assert!(other.status.success(), "{other:?}");
    assert!(still_held, "the held start was not held: {held:?}");
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
fn a_start_of_another_branch_of_the_same_fork_does_not_wait_for_one_mid_claim() {
    // Given: a start of `feat/alpha` on `demo` held inside its fetch,
    // mid-claim.
    let estate = Estate::new();
    let mut held = estate.start_held("demo", "feat/alpha", "agent-one");

    // When: a start of another branch on `demo` runs to its end. Had it
    // waited for the held start, it would have given up the wait after a
    // minute, while the held start is still held, and exited 3.
    let second = estate
        .start("demo", "feat/beta", "agent-two")
        .output()
        .expect("run the second start");
    let still_held = held.try_wait().expect("poll held").is_none();
    estate.open_gate();
    let held = finished(held, "the held start");

    // Then: it took its claim while the first was held, and both claims are
    // kept.
    assert!(second.status.success(), "{second:?}");
    assert!(still_held, "the held start was not held: {held:?}");
    assert!(held.status.success(), "{held:?}");
    let mut claims = estate.claims();
    claims.sort();
    assert_eq!(
        claims,
        [
            ("demo/feat/alpha".to_owned(), "agent-one".to_owned()),
            ("demo/feat/beta".to_owned(), "agent-two".to_owned()),
        ],
        "a claim was lost"
    );
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
    let lock = estate.home.path().join("branch-locks/demo/feat-alpha.lock");
    let waited = lab::waits_for_a_lock(&mut second, &lock);
    estate.open_gate();
    let held = finished(held, "the held start");
    let second = finished(second, "the second start");

    // Then: it waited for the first, then refused on agent-one's claim.
    assert!(
        waited,
        "the second start of one branch did not wait: {second:?}"
    );
    assert!(held.status.success(), "{held:?}");
    assert_eq!(second.status.code(), Some(2), "{second:?}");
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("agent-one"), "stderr: {stderr}");
    assert_eq!(
        estate.claims(),
        [("demo/feat/alpha".to_owned(), "agent-one".to_owned())]
    );
}

/// The forge URL `demo`'s upstream is at in the sync test.
const UPSTREAM: &str = "https://forge.invalid/acme/demo";

#[test]
fn a_sync_held_at_any_network_call_keeps_no_writer_of_its_fork_waiting() {
    // Each network call `sync` makes, by the program it runs and the
    // arguments that name it: the fetch of every remote, the forge's first
    // answer, and the read of the upstream's pull request heads.
    let forge = tempfile::tempdir().expect("forge directory");
    forge_shim::install_snapshot_gh(forge.path(), "[]", &[]);
    let phases = [
        ("the fetch", Holding::git("*\" fetch \"*"), true),
        (
            "the forge",
            Holding::new(&forge.path().join("gh"), "*"),
            false,
        ),
        (
            "the read of the pull request heads",
            Holding::git("*\" ls-remote \"*refs/pull/*"),
            true,
        ),
    ];
    for (phase, holding, no_github) in phases {
        // Given: a sync of `demo` held at that call, its upstream at a forge
        // URL, as a real fork's is, so that the forge names its repository.
        let estate = Estate::new();
        estate.demo.upstream_at_forge_url(UPSTREAM);
        let registry = estate.home.path().join("repos.toml");
        let text = std::fs::read_to_string(&registry).expect("read the registry");
        std::fs::write(
            &registry,
            text.replace(&estate.demo.upstream.display().to_string(), UPSTREAM),
        )
        .expect("write the registry");
        let mut args = vec!["--text", "sync", "demo"];
        if no_github {
            args.push("--no-github");
        }
        let mut sync = lab::knives_command(
            &estate.demo.work,
            estate.home.path(),
            estate.root.path(),
            &args,
        );
        sync.env("XDG_CACHE_HOME", forge.path().join("cache"));
        let mut held = holding
            .hold(&mut sync)
            .unwrap_or_else(|output| panic!("the sync never reached {phase}: {output:?}"));

        // When: a statement about a branch of `demo` is written, which holds
        // the fork's lock while it reads and writes. Had it waited for the
        // sync, it would have given up the wait after a minute, while the
        // sync is still held, and exited 3.
        let track = lab::knives_command(
            &estate.demo.work,
            estate.home.path(),
            estate.root.path(),
            &[
                "--text",
                "track",
                "feat/alpha",
                "--pr",
                "5",
                "--repo",
                "demo",
            ],
        )
        .output()
        .expect("run the track");
        let still_held = held.try_wait().expect("poll the held sync").is_none();
        let held = holding.release(held);

        // Then: it wrote its statement while the sync was held, and the sync
        // then ended as one with nothing to record ends.
        assert!(track.status.success(), "{phase}: {track:?}");
        assert!(still_held, "{phase}: the held sync was not held: {held:?}");
        assert!(held.status.success(), "{phase}: {held:?}");
    }
}

#[test]
fn two_syncs_of_one_fork_take_turns() {
    // Given: a sync of `demo` held inside its fetch.
    let estate = Estate::new();
    let holding = Holding::git("*\" fetch \"*");
    let mut first = lab::knives_command(
        &estate.demo.work,
        estate.home.path(),
        estate.root.path(),
        &["--text", "sync", "demo", "--no-github"],
    );
    let held = holding
        .hold(&mut first)
        .unwrap_or_else(|output| panic!("the first sync never fetched: {output:?}"));

    // When: a second sync of `demo` runs.
    let mut second = lab::knives_command(
        &estate.demo.work,
        estate.home.path(),
        estate.root.path(),
        &["--text", "sync", "demo", "--no-github"],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn the second sync");
    let waited = lab::waits_for_a_lock(
        &mut second,
        &estate.home.path().join("sync-locks/demo.lock"),
    );
    let first = holding.release(held);
    let second = finished(second, "the second sync");

    // Then: it waited for the first, so it read what the first recorded,
    // and both synced.
    assert!(waited, "the second sync did not wait: {second:?}");
    assert!(first.status.success(), "{first:?}");
    assert!(second.status.success(), "{second:?}");
}
