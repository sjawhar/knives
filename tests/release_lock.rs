//! Release writes and the fork's lock: a release write asks the network
//! first (the pull of the ledger and the consumers' pins, the forge a bare
//! `rebase` asks, the fetch an `adopt` makes) and takes its fork's lock only
//! then, reading what it decides from again under it; so another writer of
//! the fork does not wait on that network. A reap keeps a release whose
//! `start` or `finish` runs now, as it keeps one a claim holds.
//!
//! The held command is held at one network call by a program first on its
//! `PATH` that waits for a gate file ([`lab::Holding`]); each test reads that
//! the other writer ended while the held one was still held, not how long
//! either took. Had the other writer waited for the held one's lock, it would
//! have given up after a minute and exited 3, while the gate still held.

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

use std::path::PathBuf;
use std::process::Output;

use knives::jj::Repo;
use lab::{
    ConsumerForge, Holding, Lab, ReleaseOutput, commit_at, home_after_first_cut, release_command,
    release_parents, release_test_home, text,
};

/// The release the lab's first cut names.
const RELEASE: &str = "release/2026-08-04";

/// The fork's lock in the config home `home`.
fn fork_lock(home: &tempfile::TempDir) -> PathBuf {
    home.path().join("fork-locks/demo.lock")
}

/// `knives track <branch> --pr 5` on the lab fork, to its end: a writer that
/// holds the fork's lock while it reads and writes.
fn track(lab: &Lab, home: &tempfile::TempDir, branch: &str) -> Output {
    lab::knives(
        lab,
        home,
        &["--text", "track", branch, "--pr", "5", "--repo", "demo"],
    )
}

/// What a writer of the fork did while the held command waited at its gate,
/// and what the held command then did once the gate opened.
struct Witnessed {
    other: Output,
    still_held: bool,
    fork_lock_free: bool,
    held: Output,
}

/// Hold `command` at `holding`'s gate, run `other` to its end, then open the
/// gate.
fn witness(
    holding: &Holding,
    command: &mut std::process::Command,
    home: &tempfile::TempDir,
    other: impl FnOnce() -> Output,
) -> Witnessed {
    let mut held = holding
        .hold(command)
        .unwrap_or_else(|output| panic!("the command never reached its gate: {}", text(&output)));
    let other = other();
    let still_held = held.try_wait().expect("poll the held command").is_none();
    let fork_lock_free = lab::lock_is_free(&fork_lock(home));
    let held = holding.release(held);
    Witnessed {
        other,
        still_held,
        fork_lock_free,
        held,
    }
}

impl Witnessed {
    /// The other writer ended while the held command was held, which held
    /// no lock of the fork's.
    fn assert_no_wait(&self) {
        assert!(
            self.other.status.success(),
            "the other writer: {}",
            text(&self.other)
        );
        assert!(
            self.still_held,
            "the held command was not held: {}",
            text(&self.held)
        );
        assert!(
            self.fork_lock_free,
            "the held command held the fork's lock while it asked the network"
        );
    }
}

#[test]
fn an_include_held_on_the_forge_keeps_no_release_write_waiting_and_keeps_what_it_wrote() {
    // Given: a cut of alpha and beta, two branches to include, a consumer
    // the registry names, and an include of gamma held at its question to
    // the forge about that consumer.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.branch("feat/delta", "delta.txt", "delta\n");
    lab::register_consumer(home.path());
    let forge = ConsumerForge::answering();
    let holding = Holding::new(&forge.gh(), "*\" graphql \"*");
    let release = |args: &[&str]| {
        let mut command = release_command(&lab, &home, ReleaseOutput::Text, args);
        command
            .env("PATH", holding.path())
            .env("XDG_CACHE_HOME", forge.cache());
        command
    };

    // When: delta is included, to its end, while gamma's include is held.
    let seen = witness(
        &holding,
        &mut release(&["include", "feat/gamma"]),
        &home,
        || {
            release(&["include", "feat/delta"])
                .output()
                .expect("run the other include")
        },
    );

    // Then: delta went in while gamma's include was held; gamma's include,
    // which read the release under the fork's lock once the forge answered,
    // kept delta.
    seen.assert_no_wait();
    assert!(seen.held.status.success(), "{}", text(&seen.held));
    let parents = release_parents(&lab, RELEASE);
    for branch in ["feat/gamma", "feat/delta"] {
        assert!(
            parents.contains(&commit_at(&lab, branch)),
            "{branch} is not a parent of the release: {parents:?}"
        );
    }
}

#[test]
fn a_bare_rebase_held_on_the_forge_keeps_no_writer_of_its_fork_waiting() {
    // Given: a cut, and a bare rebase held at its first question to the
    // forge, which pull requests merged.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let shim = tempfile::tempdir().expect("forge directory");
    forge_shim::install_snapshot_gh(shim.path(), "[]", &[]);
    let holding = Holding::new(&shim.path().join("gh"), "*");
    let mut rebase = release_command(&lab, &home, ReleaseOutput::Text, &["rebase"]);
    rebase.env("XDG_CACHE_HOME", shim.path().join("cache"));

    // When: a statement about a branch of the fork is written, to its end.
    let seen = witness(&holding, &mut rebase, &home, || {
        track(&lab, &home, "feat/alpha")
    });

    // Then: it was written while the rebase was held; the rebase then found
    // nothing merged, as the forge said.
    seen.assert_no_wait();
    assert_eq!(seen.held.status.code(), Some(3), "{}", text(&seen.held));
    assert!(
        text(&seen.held).contains("no pull request has merged"),
        "{}",
        text(&seen.held)
    );
}

#[test]
fn an_adopt_held_at_its_fetch_keeps_no_writer_of_its_fork_waiting() {
    // Given: a cut held here only on the publish remote, and its adopt held
    // at the fetch that brings the remote's ref.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    let cut = commit_at(&lab, RELEASE);
    lab.push_branch(RELEASE);
    lab.jj_work(["bookmark", "forget", RELEASE]);
    let holding = Holding::git("*\" fetch \"*");
    let mut adopt = release_command(&lab, &home, ReleaseOutput::Text, &["adopt", RELEASE]);

    // When: a statement about a branch of the fork is written, to its end.
    let seen = witness(&holding, &mut adopt, &home, || {
        track(&lab, &home, "feat/alpha")
    });

    // Then: it was written while the adopt was held; the adopt then read the
    // publish remote's commit, which is the cut's own write.
    seen.assert_no_wait();
    assert!(seen.held.status.success(), "{}", text(&seen.held));
    assert_eq!(commit_at(&lab, &format!("{RELEASE}@origin")), cut);
}

#[test]
fn an_adopt_whose_release_lost_its_bookmark_while_it_asked_the_network_adopts_nothing() {
    // Given: a cut pushed to the publish remote, a consumer the registry
    // names, and an adopt of the cut held at its question to the forge about
    // that consumer: it read a local bookmark, so it fetched nothing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.push_branch(RELEASE);
    lab::register_consumer(home.path());
    let forge = ConsumerForge::answering();
    let holding = Holding::new(&forge.gh(), "*\" graphql \"*");
    let mut adopt = release_command(&lab, &home, ReleaseOutput::Text, &["adopt", RELEASE]);
    adopt.env("XDG_CACHE_HOME", forge.cache());
    let mut held = holding
        .hold(&mut adopt)
        .unwrap_or_else(|output| panic!("the adopt never reached its gate: {}", text(&output)));

    // When: the cut's local bookmark goes meanwhile, leaving the publish
    // remote's, and the adopt goes on.
    lab.jj_work(["bookmark", "forget", RELEASE]);
    lab.fetch_work();
    let still_held = held.try_wait().expect("poll the adopt").is_none();
    let output = holding.release(held);

    // Then: it adopts nothing from a ref no fetch of its own brought, and
    // says to run it again.
    assert!(still_held, "the adopt was not held: {}", text(&output));
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(
        text(&output).contains("this adopt fetched nothing before it took the fork's lock"),
        "{}",
        text(&output)
    );
}

#[test]
fn a_reap_keeps_a_release_whose_start_runs_and_its_cut_waits_for_none() {
    // Given: a cut, and a start of it held at its fetch: it holds the
    // release's lock, and has taken no claim yet.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let first = lab::knives_release(&lab, &home, &["cut", RELEASE]);
    assert!(first.status.success(), "{}", text(&first));
    let holding = Holding::git("*\" fetch \"*");
    let mut start = lab::start_command(&lab, &home, RELEASE);
    let mut held = holding
        .hold(&mut start)
        .unwrap_or_else(|output| panic!("the start never reached its gate: {}", text(&output)));

    // When: the next release is cut, whose reap retires the first, and then
    // a reap runs by hand, both while the start is held.
    let next = lab::knives_release(&lab, &home, &["cut", "release/2026-08-05"]);
    let reap = lab::knives_release(&lab, &home, &["reap"]);
    let kept = Repo::open(&lab.work)
        .expect("open the work checkout")
        .local_bookmark_tip(RELEASE)
        .is_some();
    let still_held = held.try_wait().expect("poll the start").is_none();
    let started = holding.release(held);

    // Then: the cut went ahead, and neither reap retired the release whose
    // start was running; the start then took its claim.
    assert!(
        text(&next).contains("cut release/2026-08-05 as "),
        "{}",
        text(&next)
    );
    for output in [&next, &reap] {
        assert!(
            text(output).contains(&format!(
                "kept {RELEASE}: a `knives start` or `finish` of it runs now"
            )),
            "{}",
            text(output)
        );
    }
    assert!(kept, "a reap forgot {RELEASE} while its start ran");
    assert!(still_held, "the start was not held: {}", text(&started));
    assert!(started.status.success(), "{}", text(&started));
}
