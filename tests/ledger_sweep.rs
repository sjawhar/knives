#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! `knives ledger sweep`, the hand-off every write makes to it, and the pull
//! a deciding command makes first, through the real binary: a config home
//! whose registry shares a fork's ledger with a repository, and whose ledger
//! root is a git repository with that repository as its `origin`, as a
//! machine set up to share its ledger has.

#[path = "common/lab.rs"]
mod lab;

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use knives::ledger::{Entry, Kind, Ledger};

/// The forge repository the lab fork's upstream is reached by in these
/// tests. A filesystem upstream names no repository another machine could
/// share, so its fork is kept under its registry key, and the registry
/// refuses a `ledger` for it.
const UPSTREAM: &str = "https://forge.invalid/acme/demo";
/// [`UPSTREAM`]'s `<owner>/<name>`: what the ledger keeps the lab fork under.
const FORK: &str = "acme/demo";
/// The repository each fork's registry entry says its ledger belongs to.
const LEDGER: &str = "acme/ledger";
/// [`LEDGER`] as each ledger root's `origin` spells it; git reaches it at a
/// bare remote through `insteadOf`.
const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

/// A config home whose registry calls the lab fork `demo`, with [`UPSTREAM`]
/// as its upstream and its ledger shared with [`LEDGER`], and the lab's work
/// checkout pointing there too.
fn shared_home(lab: &lab::Lab) -> tempfile::TempDir {
    let (home, _consumer) = lab::release_test_home(lab);
    lab.upstream_at_forge_url(UPSTREAM);
    let registry = home.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!(
            "{}ledger = \"{LEDGER}\"\n",
            text.replace(&lab.upstream.display().to_string(), UPSTREAM)
        ),
    )
    .expect("write the registry");
    home
}

/// A config home whose registry names one fork, kept under `acme/a-repo`,
/// sharing its ledger with [`LEDGER`].
fn a_repo_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
             origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"{LEDGER}\"\n"
        ),
    )
    .expect("write the registry");
    home
}

/// Make `home`'s ledger root a repository whose `origin` is [`LEDGER_URL`],
/// reached at `remote`, committing as `machine`; return the root.
fn share(home: &Path, machine: &str, remote: &Path) -> PathBuf {
    let root = home.join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);
    let reached = remote.to_str().expect("utf-8");
    lab::git_output(
        &root,
        ["config", &format!("url.{reached}.insteadOf"), LEDGER_URL],
    );
    lab::git_output(&root, ["config", "knives.machine", machine]);
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
                email: None,
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
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 12);

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
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);
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
    // another whose repository names no machine, with a registry sharing no
    // fork's ledger.
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
    // Given: a registry sharing a fork's ledger with the root's origin, and
    // a root that names no machine.
    let home = a_repo_home();
    let root = home.path().join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);

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
fn a_fork_whose_ledger_belongs_elsewhere_is_carried_nowhere_and_said_so() {
    // Given: a registry sharing a fork's ledger with one repository, a root
    // whose origin is another, and an entry of that fork's on disk.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
         origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"acme/elsewhere\"\n",
    )
    .expect("write the registry");
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);

    // When: a sweep runs.
    let swept = sweep(home.path());

    // Then: it fails naming the repository the registry says, carries the
    // entry nowhere, and leaves the reason in the log.
    assert_eq!(swept.status.code(), Some(3), "{swept:?}");
    let report: serde_json::Value = serde_json::from_slice(&swept.stdout).expect("JSON");
    let said = "repos.toml says its ledger belongs to acme/elsewhere";
    assert!(report["problems"].to_string().contains(said), "{report}");
    assert_eq!(entries_on(&remote, "alpha"), 0);
    let log = std::fs::read_to_string(home.path().join("ledger-sweep.log")).expect("a log");
    assert!(log.contains(said), "{log}");
}

#[test]
fn a_write_hands_off_to_a_sweep_and_does_not_wait_for_it() {
    // Given: a managed fork on a machine that shares its ledger, with the
    // ledger repository's transport lock held, so any sweep stalls in its
    // first pass until the test lets go.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
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
    let home = shared_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    share(home.path(), "alpha", &remote);

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

/// `knives <args>` from `lab`'s work checkout, as the machine whose config
/// home is `home`.
fn knives_on(lab: &lab::Lab, home: &Path, args: &[&str]) -> Output {
    lab::knives_command(&lab.work, home, lab.temp_path(), args)
        .env("KNIVES_OWNER", "ses_fff688")
        .output()
        .expect("run knives")
}

/// `status demo` as JSON, without the forge or landed probes.
fn status_on(lab: &lab::Lab, home: &Path) -> (Option<i32>, serde_json::Value) {
    let status = knives_on(
        lab,
        home,
        &["--json", "status", "demo", "--no-github", "--no-landed"],
    );
    let report = serde_json::from_slice(&status.stdout).unwrap_or_else(|error| {
        panic!(
            "status emits JSON ({error}): {}\n{}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        )
    });
    (status.status.code(), report)
}

/// A second machine's config home: the same registry, nothing else yet.
fn second_machine(first: &Path) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("second config home");
    std::fs::copy(first.join("repos.toml"), home.path().join("repos.toml"))
        .expect("copy the registry");
    home
}

#[test]
fn a_statement_made_on_one_machine_shows_in_status_on_another_after_a_sweep() {
    // Given: two machines sharing one ledger remote, each with its own config
    // home and ledger, and a branch both can see.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);

    // When: alpha states the branch's pull request, and the sweep that write
    // handed off has carried it to the remote.
    let tracked = knives_on(
        &lab,
        alpha.path(),
        &["--text", "track", "feat/alpha", "--pr", "1234"],
    );
    assert!(
        tracked.status.success(),
        "{}",
        String::from_utf8_lossy(&tracked.stderr)
    );
    wait_until("alpha's statement to reach the remote", || {
        entries_on(&remote, "alpha") == 1
    });
    settle(alpha.path());
    assert!(
        !beta.path().join("ledger").join(FORK).exists(),
        "beta had alpha's entry before it asked"
    );

    // Then: beta's status, with no sync or sweep of its own, shows the
    // statement on the branch's row, and reports no problem.
    let (code, report) = status_on(&lab, beta.path());
    assert_eq!(code, Some(0), "{report}");
    let row = report["branches"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["name"] == "feat/alpha")
        .unwrap_or_else(|| panic!("no feat/alpha row: {report}"));
    assert_eq!(row["pr"]["number"], 1234, "row was: {row}");
    assert_eq!(row["pr"]["stated"], true, "row was: {row}");
    assert!(report.get("problems").is_none(), "{report}");
}

#[test]
fn a_status_that_cannot_pull_says_so_and_still_answers() {
    // Given: a machine whose ledger remote cannot be reached.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = shared_home(&lab);
    share(home.path(), "alpha", Path::new("/nonexistent/ledger.git"));

    // When: status runs.
    let (code, report) = status_on(&lab, home.path());

    // Then: the failed pull is a problem, and the branches are still reported.
    assert_eq!(code, Some(3), "{report}");
    let problems = report["problems"].as_array().expect("problems");
    assert!(
        problems.iter().any(|problem| problem
            .as_str()
            .is_some_and(|text| text.contains("could not pull the ledger from origin"))),
        "{report}"
    );
    assert!(
        report["branches"]
            .as_array()
            .expect("branch rows")
            .iter()
            .any(|row| row["name"] == "feat/alpha"),
        "{report}"
    );
}

#[test]
fn status_counts_entries_the_remote_lacks_and_names_a_failed_sweeps_log() {
    // Given: a shared ledger with one entry no sweep has carried.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, FORK, 1);

    // When: status runs.
    let (code, report) = status_on(&lab, home.path());

    // Then: a note says one entry is not on the remote yet; nothing failed.
    assert_eq!(code, Some(0), "{report}");
    let notes = |report: &serde_json::Value| -> Vec<String> {
        report["notes"]
            .as_array()
            .map(|notes| {
                notes
                    .iter()
                    .filter_map(|note| note.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        notes(&report)
            .iter()
            .any(|note| note.starts_with("1 ledger entry not yet on origin")),
        "{report}"
    );

    // When: the last sweep failed, and status runs again.
    let log = home.path().join("ledger-sweep.log");
    std::fs::write(&log, "the last ledger sweep failed:\n").expect("write a sweep log");
    let (code, report) = status_on(&lab, home.path());

    // Then: the same note names where the failure is.
    assert_eq!(code, Some(0), "{report}");
    let named = format!("see {}", log.display());
    assert!(
        notes(&report)
            .iter()
            .any(|note| note.starts_with("1 ledger entry not yet on origin")
                && note.contains(&named)),
        "{report}"
    );
}

#[test]
fn a_release_cut_is_refused_when_the_ledger_cannot_be_pulled() {
    // Given: a branch ready for a first cut, on a machine whose ledger remote
    // cannot be reached.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = shared_home(&lab);
    share(home.path(), "alpha", Path::new("/nonexistent/ledger.git"));

    // When: the cut is asked for.
    let cut = lab::knives_release(&lab, &home, &["cut", "release/2026-08-04"]);

    // Then: it is refused as incomplete, saying why, and no release exists.
    let stdout = String::from_utf8_lossy(&cut.stdout);
    assert_eq!(cut.status.code(), Some(3), "{stdout}");
    assert!(
        stdout.contains("could not pull the ledger from origin"),
        "{stdout}"
    );
    assert!(
        knives::jj::Repo::open(&lab.work)
            .expect("open the work checkout")
            .resolve_commit("release/2026-08-04")
            .is_err(),
        "the cut was made anyway"
    );
}
