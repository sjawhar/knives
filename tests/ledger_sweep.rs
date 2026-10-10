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

#[path = "common/forge_shim.rs"]
mod forge_shim;
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
    share_at(home, machine, remote.to_str().expect("utf-8"))
}

/// [`share`], with [`LEDGER_URL`] reached at the URL or path `reached`.
fn share_at(home: &Path, machine: &str, reached: &str) -> PathBuf {
    let root = home.join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);
    lab::git_output(
        &root,
        ["config", &format!("url.{reached}.insteadOf"), LEDGER_URL],
    );
    lab::git_output(&root, ["config", "knives.machine", machine]);
    root
}

/// The URL of a remote that takes every connection and never sends a byte.
fn stalled_remote() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("a local address").port();
    std::thread::spawn(move || {
        // Collecting never finishes: each accepted stream is kept open,
        // unanswered, until the test process exits.
        let open: Vec<_> = listener.incoming().collect();
        drop(open);
    });
    format!("http://127.0.0.1:{port}/acme/ledger")
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
    // And: each sweep either carried the burst or found it being carried.
    // Which, and how many of each, is up to the scheduler.
    let outcomes: Vec<String> = outputs
        .iter()
        .map(|output| {
            let report: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("a sweep reports JSON");
            report["outcome"].as_str().expect("an outcome").to_owned()
        })
        .collect();
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome == "busy" || outcome == "swept"),
        "{outcomes:?}"
    );
}

#[test]
fn a_sweep_that_finds_the_lock_held_leaves_at_once_as_busy() {
    // Given: a machine with an entry to send, and its ledger lock held, as
    // a running sweep holds it.
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);
    let held = hold_sweep_lock(home.path());

    // When: a sweep runs.
    let swept = sweep(home.path());

    // Then: it succeeds as busy, having sent nothing for the holder to race.
    assert!(swept.status.success(), "{swept:?}");
    let report: serde_json::Value = serde_json::from_slice(&swept.stdout).expect("JSON");
    assert_eq!(report["outcome"], "busy", "{report}");
    assert_eq!(entries_on(&remote, "alpha"), 0);
    drop(held);
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
fn a_sweep_reaches_the_remote_through_the_git_config_its_session_hands_it() {
    // Given: a machine whose ledger root names its origin by a URL only the
    // session's own git configuration rewrites to where the remote is, as a
    // session routes git to the identity it acts as with an `include.path`
    // handed over in `GIT_CONFIG_COUNT` and its numbered pairs.
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let root = home.path().join("ledger");
    lab::git_repository(&root, &[("origin", LEDGER_URL)]);
    lab::git_output(&root, ["config", "knives.machine", "alpha"]);
    let routes = home.path().join("routes.gitconfig");
    std::fs::write(
        &routes,
        format!(
            "[url \"{}\"]\n\tinsteadOf = {LEDGER_URL}\n",
            remote.display()
        ),
    )
    .expect("write the session's git config");
    append(&root, "acme/a-repo", 1);
    // An inherited GIT_DIR naming some other repository, as a git hook
    // exports one, which the transport must still ignore.
    let elsewhere = tempfile::tempdir().expect("another repository");
    lab::git_repository(elsewhere.path(), &[]);
    let sweep_as_session = |include: Option<&Path>| {
        let mut command = lab::knives_command(
            home.path(),
            home.path(),
            home.path(),
            &["--json", "ledger", "sweep"],
        );
        command
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .env("GIT_DIR", elsewhere.path().join(".git"));
        if let Some(include) = include {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "include.path")
                .env("GIT_CONFIG_VALUE_0", include);
        }
        command.output().expect("run knives ledger sweep")
    };

    // When: a sweep runs without that configuration.
    let without = sweep_as_session(None);

    // Then: it cannot reach the remote, which has nothing.
    assert_eq!(
        without.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&without.stdout)
    );
    assert_eq!(entries_on(&remote, "alpha"), 0);

    // When: the session hands it over.
    let with = sweep_as_session(Some(&routes));

    // Then: the sweep reaches the remote through it and the entry is there.
    assert!(
        with.status.success(),
        "{}",
        String::from_utf8_lossy(&with.stdout)
    );
    assert_eq!(entries_on(&remote, "alpha"), 1);
}

#[test]
fn a_ledger_nobody_set_up_to_share_is_swept_as_nothing_and_touched_nowhere() {
    // Given: a config home whose ledger root is no repository at all, and
    // another whose repository names no machine, with a registry sharing no
    // fork's ledger and an entry of that fork's on disk.
    for initialised in [false, true] {
        let home = tempfile::tempdir().expect("config home");
        std::fs::write(
            home.path().join("repos.toml"),
            "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
             origin = \"https://forge.invalid/ours/a-repo\"\n",
        )
        .expect("write the registry");
        let root = home.path().join("ledger");
        if initialised {
            lab::git_repository(&root, &[]);
        }
        append(&root, "acme/a-repo", 1);

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

    // Then: it fails, says so in its outcome, names the command that fixes
    // it, and leaves that in the log.
    assert_eq!(swept.status.code(), Some(3), "{swept:?}");
    let report: serde_json::Value = serde_json::from_slice(&swept.stdout).expect("JSON");
    assert_eq!(report["outcome"], "failed", "{report}");
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
fn one_ledger_root_sweeps_each_fork_to_the_repository_it_names_and_no_other() {
    // Given: one ledger root, two forks, one sharing its ledger with the
    // company repository through the root's own `.git`, and one with a
    // dot-named personal repository through a second git directory over
    // the same root, and entries of both on disk.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
             origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"{LEDGER}\"\n\n\
             [repos.b-repo]\nupstream = \"https://forge.invalid/acme/b-repo\"\n\
             origin = \"https://forge.invalid/ours/b-repo\"\n\
             ledger = \"someone/.knives-ledger\"\n"
        ),
    )
    .expect("write the registry");
    let (_company_dir, company) = bare_remote();
    let (_personal_dir, personal) = bare_remote();
    let root = share(home.path(), "alpha", &company);
    let personal_url = "https://forge.invalid/someone/.knives-ledger";
    let second = home.path().join("ledger-repositories").join("personal.git");
    lab::git_bare_repository(&second);
    for args in [
        vec!["config", "core.bare", "false"],
        vec!["remote", "add", "origin", personal_url],
        vec![
            "config",
            &format!("url.{}.insteadOf", personal.display()),
            personal_url,
        ],
        vec!["config", "knives.machine", "alpha"],
    ] {
        assert!(
            git_in(&second, &args).is_some(),
            "git {args:?} in the second git directory"
        );
    }
    append(&root, "acme/a-repo", 2);
    append(&root, "acme/b-repo", 3);

    // When: one sweep runs.
    let swept = sweep(home.path());

    // Then: it succeeded through both, each remote holds exactly its own
    // fork's entries, and neither holds the other's.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert!(swept.status.success(), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("a sweep report");
    assert_eq!(
        report["destinations"].as_array().map(Vec::len),
        Some(2),
        "{report}"
    );
    for (remote, fork, count) in [(&company, "acme/a-repo", 2), (&personal, "acme/b-repo", 3)] {
        let listing = git_in(
            remote,
            &["ls-tree", "-r", "--name-only", "refs/knives/alpha"],
        )
        .expect("the machine's ref on the remote");
        let entries: Vec<&str> = listing.lines().filter(|path| is_entry(path)).collect();
        assert_eq!(entries.len(), count, "{}: {entries:?}", remote.display());
        assert!(
            entries
                .iter()
                .all(|path| path.starts_with(&format!("{fork}/"))),
            "{} holds another fork's entries: {entries:?}",
            remote.display()
        );
    }
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

/// Join `home` to [`LEDGER`] as the dotfiles installer does on a new
/// machine: a git directory in `ledger-repositories` whose work tree is the
/// ledger root, which nothing has created yet, reaching [`LEDGER_URL`] at
/// `remote` and committing as `machine`.
fn join_as_the_installer_does(home: &Path, machine: &str, remote: &Path) {
    let git_dir = home.join("ledger-repositories").join("acme--ledger.git");
    lab::git_bare_repository(&git_dir);
    let work_tree = home.join("ledger");
    for args in [
        vec!["config", "core.bare", "false"],
        vec![
            "config",
            "core.worktree",
            work_tree.to_str().expect("utf-8"),
        ],
        vec!["remote", "add", "origin", LEDGER_URL],
        vec![
            "config",
            &format!("url.{}.insteadOf", remote.display()),
            LEDGER_URL,
        ],
        vec!["config", "knives.machine", machine],
    ] {
        assert!(git_in(&git_dir, &args).is_some(), "git {args:?}");
    }
}

#[test]
fn a_new_machine_with_no_ledger_root_yet_pulls_what_another_stated() {
    // Given: alpha's statement on the remote, and beta joined as the
    // installer joins a new machine, with no ledger root at all.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = bare_remote();
    share(alpha.path(), "alpha", &remote);
    join_as_the_installer_does(beta.path(), "beta", &remote);
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
    assert!(!beta.path().join("ledger").exists());

    // When: beta's status runs, pulling first.
    let (code, report) = status_on(&lab, beta.path());

    // Then: it pulled alpha's statement, and reports no problem.
    assert_eq!(code, Some(0), "{report}");
    let row = report["branches"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["name"] == "feat/alpha")
        .unwrap_or_else(|| panic!("no feat/alpha row: {report}"));
    assert_eq!(row["pr"]["number"], 1234, "row was: {row}");
    assert!(report.get("problems").is_none(), "{report}");

    // And: beta's own sweep goes through the same git directory cleanly.
    let swept = sweep(beta.path());
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
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
fn a_status_whose_ledger_remote_stalls_still_answers_within_a_bound() {
    // Given: a machine whose ledger remote accepts the connection and never answers.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = shared_home(&lab);
    share_at(home.path(), "alpha", &stalled_remote());

    // When: status runs.
    let started = Instant::now();
    let (code, report) = status_on(&lab, home.path());

    // Then: it answers well inside a minute, with the stalled pull as a problem.
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "status hung for {:?}",
        started.elapsed()
    );
    assert_eq!(code, Some(3), "{report}");
    assert!(
        report["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .any(|problem| problem
                .as_str()
                .is_some_and(|text| text.contains("could not pull the ledger from origin"))),
        "{report}"
    );
}

#[test]
fn a_sweep_whose_ledger_remote_stalls_gives_up_within_a_bound_and_lets_the_lock_go() {
    // Given: a machine with an entry to send, whose ledger remote accepts the
    // connection and never answers.
    let home = a_repo_home();
    let root = share_at(home.path(), "alpha", &stalled_remote());
    append(&root, "acme/a-repo", 1);

    // When: a sweep runs.
    let started = Instant::now();
    let swept = sweep(home.path());

    // Then: it fails well inside a minute, says why, records it in the
    // sweep log, and no longer holds the ledger's lock.
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the sweep hung for {:?}",
        started.elapsed()
    );
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert_eq!(swept.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("origin"), "{stdout}");
    assert!(home.path().join("ledger-sweep.log").exists());
    assert!(!held(&home.path().join("ledger.lock")));
}

#[test]
fn a_dependency_stated_on_another_machine_survives_a_second_machines_depends() {
    // Given: two machines sharing one ledger remote, and alpha's dependency
    // on the remote, which beta has never pulled.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let stated = knives_on(
        &lab,
        alpha.path(),
        &["--text", "depends", "feat/alpha", "--on", "demo#1"],
    );
    assert!(
        stated.status.success(),
        "{}",
        String::from_utf8_lossy(&stated.stderr)
    );
    wait_until("alpha's statement to reach the remote", || {
        entries_on(&remote, "alpha") == 1
    });
    settle(alpha.path());

    // When: beta states another dependency of the same branch.
    let stated = knives_on(
        &lab,
        beta.path(),
        &["--text", "depends", "feat/alpha", "--on", "demo#2"],
    );
    assert!(
        stated.status.success(),
        "{}",
        String::from_utf8_lossy(&stated.stderr)
    );
    settle(beta.path());

    // Then: beta's statement, the newest, lists both.
    let entries = Ledger::at(beta.path().join("ledger").join(FORK))
        .entries()
        .expect("read beta's ledger");
    let live = knives::statement::Statements::from_entries(&entries);
    assert_eq!(live.depends("feat/alpha"), ["acme/demo#1", "acme/demo#2"]);
}

#[test]
fn a_forget_with_nothing_stated_leaves_a_peers_unsent_statement_standing() {
    // Given: two machines sharing one ledger remote, and a pull request
    // alpha stated whose sweep has not yet carried it.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let held = hold_sweep_lock(alpha.path());
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

    // When: beta, which can see no statement, forgets one, and then alpha's
    // statement reaches the remote.
    let forgot = knives_on(
        &lab,
        beta.path(),
        &["--text", "track", "feat/alpha", "--forget"],
    );
    assert!(
        forgot.status.success(),
        "{}",
        String::from_utf8_lossy(&forgot.stderr)
    );
    settle(beta.path());
    drop(held);
    assert!(sweep(alpha.path()).status.success());
    settle(alpha.path());

    // Then: beta wrote no forget, so once it pulls, alpha's #1234 is live
    // on beta too.
    let entries = Ledger::at(beta.path().join("ledger").join(FORK))
        .entries()
        .expect("read beta's ledger");
    assert!(
        entries.iter().all(|entry| entry.statement.is_none()),
        "{entries:?}"
    );
    let (code, report) = status_on(&lab, beta.path());
    assert_eq!(code, Some(0), "{report}");
    let row = report["branches"]
        .as_array()
        .expect("branch rows")
        .iter()
        .find(|row| row["name"] == "feat/alpha")
        .unwrap_or_else(|| panic!("no feat/alpha row: {report}"));
    assert_eq!(row["pr"]["number"], 1234, "row was: {row}");
}

#[test]
fn a_depends_or_a_forget_is_refused_when_the_ledger_cannot_be_pulled() {
    // Given: a machine whose ledger remote cannot be reached.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = shared_home(&lab);
    share(home.path(), "alpha", Path::new("/nonexistent/ledger.git"));

    // When / Then: each write that replaces what it read is refused, says
    // why, and writes nothing.
    for args in [
        &["--text", "depends", "feat/alpha", "--on", "demo#1"][..],
        &["--text", "track", "feat/alpha", "--forget"][..],
    ] {
        let refused = knives_on(&lab, home.path(), args);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(3), "{args:?}: {stderr}");
        assert!(
            stderr.contains("could not pull the ledger from origin"),
            "{args:?}: {stderr}"
        );
        assert!(
            Ledger::at(home.path().join("ledger").join(FORK))
                .entries()
                .expect("read the ledger")
                .is_empty(),
            "{args:?} wrote an entry"
        );
    }

    // And: stating a pull request, which replaces nothing it read, still
    // records.
    let tracked = knives_on(
        &lab,
        home.path(),
        &["--text", "track", "feat/alpha", "--pr", "7"],
    );
    assert!(
        tracked.status.success(),
        "{}",
        String::from_utf8_lossy(&tracked.stderr)
    );
    settle(home.path());
}

#[test]
fn a_handed_off_sweep_whose_ledger_remote_stalls_lets_the_lock_go_within_a_bound() {
    // Given: a managed fork on a machine whose ledger remote accepts the
    // connection and never answers.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    share_at(home.path(), "alpha", &stalled_remote());

    // When: a note is written, handing off a sweep.
    let wrote = knives_on(
        &lab,
        home.path(),
        &["--text", "notch", "-m", "stalled", "--repo", "demo"],
    );
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );

    // Then: within a minute the sweep has given up, recorded why in the
    // sweep log, and let the ledger's lock go.
    let log = home.path().join("ledger-sweep.log");
    wait_until("the stalled sweep to give up", || {
        log.exists() && !held(&home.path().join("ledger.lock"))
    });
    let recorded = std::fs::read_to_string(&log).expect("read the sweep log");
    assert!(recorded.contains("origin"), "{recorded}");
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

/// The notes of a status report.
fn notes(report: &serde_json::Value) -> Vec<String> {
    report["notes"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(|note| note.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn status_names_the_peer_entries_a_machine_does_not_carry_the_fork_for() {
    // Given: alpha shares the lab fork's ledger with acme/ledger; beta
    // shares acme/ledger too, for another fork, and keeps the lab fork's
    // ledger to itself.
    let lab = lab::Lab::new();
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let registry = beta.path().join("repos.toml");
    let text = std::fs::read_to_string(&registry).expect("read the registry");
    std::fs::write(
        &registry,
        format!(
            "{}\n[repos.other]\nupstream = \"https://forge.invalid/acme/other\"\n\
             origin = \"https://forge.invalid/ours/other\"\nledger = \"{LEDGER}\"\n",
            text.replace(&format!("ledger = \"{LEDGER}\"\n"), "")
        ),
    )
    .expect("write the registry");
    let (_remote_dir, remote) = bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let wrote = knives_on(
        &lab,
        alpha.path(),
        &["--text", "notch", "-m", "from alpha", "--repo", "demo"],
    );
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    wait_until("alpha's note to reach the remote", || {
        entries_on(&remote, "alpha") == 1
    });
    settle(alpha.path());

    // When: beta sweeps.
    let swept = sweep(beta.path());

    // Then: the sweep counts alpha's note as left unread, and writes none of it.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert!(swept.status.success(), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("a sweep report");
    assert_eq!(
        report["destinations"][0]["skipped"],
        serde_json::json!({ FORK: 1 }),
        "{report}"
    );
    assert!(!beta.path().join("ledger").join(FORK).exists());

    // And: beta's status of the lab fork says where that entry is and why
    // it is not read, though no sweep's output reaches anyone.
    let (code, status) = status_on(&lab, beta.path());
    assert_eq!(code, Some(0), "{status}");
    let through = format!(
        "1 entry other machines sent through origin ({}) is not read here",
        beta.path().join("ledger").join(".git").display()
    );
    assert!(
        notes(&status).iter().any(|note| note.contains(&through)),
        "{status}"
    );
}

#[test]
fn one_unreadable_entry_does_not_hold_back_the_rest_of_a_sweep() {
    // Given: a repository carrying two forks, each with an entry to send,
    // and one more entry in the first that does not parse.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
             origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"{LEDGER}\"\n\n\
             [repos.b-repo]\nupstream = \"https://forge.invalid/acme/b-repo\"\n\
             origin = \"https://forge.invalid/ours/b-repo\"\nledger = \"{LEDGER}\"\n"
        ),
    )
    .expect("write the registry");
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);
    append(&root, "acme/b-repo", 1);
    let broken = root
        .join("acme/a-repo")
        .join("20261010T120000.000000000Z-0000.md");
    std::fs::write(&broken, "not an entry\n").expect("write a broken entry");

    // When: a sweep runs.
    let swept = sweep(home.path());

    // Then: the two good entries went out together, and the broken one is
    // held back, named in the sweep's problems, where it can still be fixed.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert_eq!(swept.status.code(), Some(3), "{stdout}");
    assert_eq!(entries_on(&remote, "alpha"), 2, "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("a sweep report");
    assert_eq!(report["destinations"][0]["pushes"], 1, "{report}");
    let problems = report["problems"].to_string();
    assert!(
        problems.contains(&broken.display().to_string()) && problems.contains("not sent"),
        "{report}"
    );

    // When: the broken entry is removed, and the machine sweeps again.
    std::fs::remove_file(&broken).expect("remove the broken entry");
    let swept = sweep(home.path());

    // Then: it succeeds, with nothing left behind.
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    assert!(!home.path().join("ledger-sweep.log").exists());
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

#[test]
fn release_members_pulls_first_and_says_when_it_could_not() {
    // Given: a cut release, on a machine whose ledger remote then becomes
    // unreachable.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = shared_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    let cut = lab::knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
    settle(home.path());
    let reached = remote.to_str().expect("utf-8");
    lab::git_output(
        &root,
        ["config", "--unset-all", &format!("url.{reached}.insteadOf")],
    );
    lab::git_output(
        &root,
        [
            "config",
            "url./nonexistent/ledger.git.insteadOf",
            LEDGER_URL,
        ],
    );

    // When: members is asked about the release in hand.
    let members = lab::knives_release(&lab, &home, &["members"]);

    // Then: it still lists the release's parents, and the failed pull is a
    // problem that makes the answer incomplete.
    let stdout = String::from_utf8_lossy(&members.stdout);
    assert_eq!(members.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("release/2026-08-04"), "{stdout}");
    assert!(
        stdout.contains("could not pull the ledger from origin"),
        "{stdout}"
    );
}

#[test]
fn each_release_edit_is_refused_when_the_ledger_cannot_be_pulled() {
    // Given: a cut release, on a machine whose ledger remote then becomes
    // unreachable, and a branch that could be included.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = shared_home(&lab);
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    let cut = lab::knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
    settle(home.path());
    let reached = remote.to_str().expect("utf-8");
    lab::git_output(
        &root,
        ["config", "--unset-all", &format!("url.{reached}.insteadOf")],
    );
    lab::git_output(
        &root,
        [
            "config",
            "url./nonexistent/ledger.git.insteadOf",
            LEDGER_URL,
        ],
    );
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let before = knives::jj::Repo::open(&lab.work)
        .expect("open the work checkout")
        .resolve_commit("release/2026-08-04")
        .expect("the release");

    // When / Then: every edit is refused as incomplete, says why, and leaves
    // the release where it was.
    for args in [
        &["include", "feat/gamma"][..],
        &["drop", "feat/alpha", "--why", "not this time"][..],
        &["advance"][..],
        &["rebase", "main@upstream"][..],
    ] {
        let edit = lab::knives_release(&lab, &home, args);
        let stdout = String::from_utf8_lossy(&edit.stdout);
        assert_eq!(edit.status.code(), Some(3), "{args:?}: {stdout}");
        assert!(
            stdout.contains("could not pull the ledger from origin"),
            "{args:?}: {stdout}"
        );
        assert_eq!(
            knives::jj::Repo::open(&lab.work)
                .expect("open the work checkout")
                .resolve_commit("release/2026-08-04")
                .expect("the release"),
            before,
            "{args:?} moved the release"
        );
    }
}

#[test]
fn a_sweep_refuses_a_ledger_still_kept_under_the_registry_key() {
    // Given: a shared fork whose entries still sit under its registry key.
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "a-repo", 1);

    // When: a sweep runs.
    let swept = sweep(home.path());

    // Then: it fails naming the migration, sends nothing, and says so in its log.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert_eq!(swept.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("knives ledger migrate"), "{stdout}");
    assert_eq!(entries_on(&remote, "alpha"), 0);
    let log = std::fs::read_to_string(home.path().join("ledger-sweep.log")).expect("a log");
    assert!(log.contains("knives ledger migrate"), "{log}");
}

#[test]
fn a_ledger_root_whose_git_is_a_gitfile_sweeps_through_the_directory_it_names() {
    // Given: a ledger root whose `.git` is a file naming a git directory
    // elsewhere, as `git init --separate-git-dir` or a linked worktree
    // leaves it, with an entry to send.
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let elsewhere = tempfile::tempdir().expect("a directory for the git directory");
    let git_dir = elsewhere.path().join("ledger.git");
    let root = home.path().join("ledger");
    lab::git_output(
        home.path(),
        [
            "init",
            "--quiet",
            "--separate-git-dir",
            git_dir.to_str().expect("utf-8"),
            root.to_str().expect("utf-8"),
        ],
    );
    assert!(root.join(".git").is_file(), "the root's .git is a gitfile");
    lab::git_output(&root, ["remote", "add", "origin", LEDGER_URL]);
    lab::git_output(
        &root,
        [
            "config",
            &format!("url.{}.insteadOf", remote.display()),
            LEDGER_URL,
        ],
    );
    lab::git_output(&root, ["config", "knives.machine", "alpha"]);
    append(&root, "acme/a-repo", 1);

    // When: a sweep runs.
    let swept = sweep(home.path());

    // Then: it sent the entry, through the git directory the gitfile names,
    // and took its transport lock there.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert!(swept.status.success(), "{stdout}");
    assert_eq!(entries_on(&remote, "alpha"), 1, "{stdout}");
    assert!(git_dir.join("knives-transport.lock").is_file(), "{stdout}");
}

#[test]
fn a_recreated_checkout_sweeping_as_an_existing_machine_is_refused_and_pushes_nothing() {
    // Given: alpha's entry on the remote, then a fresh checkout that names
    // itself alpha too, with an entry of its own.
    let home = a_repo_home();
    let (_remote_dir, remote) = bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);
    assert!(sweep(home.path()).status.success());
    let pushed = git_in(&remote, &["rev-parse", "refs/knives/alpha"]).expect("alpha's ref");
    let again = a_repo_home();
    let again_root = share(again.path(), "alpha", &remote);
    append(&again_root, "acme/a-repo", 1);

    // When: the fresh checkout sweeps.
    let swept = sweep(again.path());

    // Then: it refuses to write as alpha, and the remote's ref is untouched.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert_eq!(swept.status.code(), Some(3), "{stdout}");
    assert!(stdout.contains("refusing to write as alpha"), "{stdout}");
    assert_eq!(
        git_in(&remote, &["rev-parse", "refs/knives/alpha"]),
        Some(pushed)
    );
    assert_eq!(
        git_in(
            &again_root.join(".git"),
            &["rev-parse", "--verify", "refs/knives/alpha"]
        ),
        None,
        "the fresh checkout started a history of its own as alpha"
    );
}

#[test]
fn a_peer_ref_that_moved_backward_keeps_out_only_that_machines_entries() {
    // Given: three machines sharing one ledger remote. beta has fetched
    // gamma's ref at its second commit, and then the remote's copy of
    // gamma's ref was put back to its first, as a force-push or a deleted
    // and re-pushed ref leaves it.
    let lab = lab::Lab::new();
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let gamma = second_machine(alpha.path());
    let (_remote_dir, remote) = bare_remote();
    let alpha_root = share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let gamma_root = share(gamma.path(), "gamma", &remote);
    append(&gamma_root, FORK, 1);
    assert!(sweep(gamma.path()).status.success());
    let first = git_in(&remote, &["rev-parse", "refs/knives/gamma"]).expect("gamma's ref");
    append(&gamma_root, FORK, 1);
    assert!(sweep(gamma.path()).status.success());
    assert!(sweep(beta.path()).status.success());
    git_in(&remote, &["update-ref", "refs/knives/gamma", &first]).expect("rewind gamma's ref");
    let beta_entries = || {
        Ledger::at(beta.path().join("ledger").join(FORK))
            .entries()
            .expect("beta's ledger")
            .len()
    };
    assert_eq!(beta_entries(), 2);

    // When: alpha sends an entry, and beta sweeps.
    append(&alpha_root, FORK, 1);
    assert!(sweep(alpha.path()).status.success());
    let swept = sweep(beta.path());

    // Then: the sweep fails naming gamma's ref, and writes in alpha's entry.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert_eq!(swept.status.code(), Some(3), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("a sweep report");
    assert!(
        report["problems"].to_string().contains("refs/knives/gamma"),
        "{report}"
    );
    assert_eq!(report["destinations"][0]["pulled"], 1, "{report}");
    assert_eq!(beta_entries(), 3);

    // When: alpha sends another, and beta's status pulls before it reports.
    append(&alpha_root, FORK, 1);
    assert!(sweep(alpha.path()).status.success());
    let (code, status) = status_on(&lab, beta.path());

    // Then: that pull writes it in too, and says what it could not fetch.
    assert_eq!(code, Some(3), "{status}");
    assert!(
        status["problems"].to_string().contains("refs/knives/gamma"),
        "{status}"
    );
    assert_eq!(beta_entries(), 4);
}

/// The transition both machines see in these tests.
const MERGED: &str = "#7 merged";

/// A fake forge reporting pull request #7, from `feat/alpha`, in `state`.
fn forge_reporting(state: &str) -> tempfile::TempDir {
    let shim = tempfile::tempdir().expect("forge shim directory");
    forge_shim::install_snapshot_gh(
        shim.path(),
        &format!(
            "[{}]",
            forge_shim::pull_record(7, state, "feat/alpha", None)
        ),
        &[],
    );
    shim
}

/// `knives sync demo` on the machine whose config home is `home`, asking
/// the forge `shim`, with a cache of its own so every run reads the forge.
fn sync_on(lab: &lab::Lab, home: &Path, shim: &Path) -> Output {
    let cache = tempfile::tempdir().expect("forge cache");
    lab::knives_command(
        &lab.work,
        home,
        lab.temp_path(),
        &["--json", "sync", "demo"],
    )
    .env("KNIVES_OWNER", "ses_fff688")
    .env("PATH", forge_shim::path_with_gh_shim(shim))
    .env("XDG_CACHE_HOME", cache.path())
    .output()
    .expect("run knives sync")
}

/// How many entry files across every machine's ref on `remote` have a line
/// reading exactly `text`, which holds no regular-expression syntax.
fn recorded_on(remote: &Path, text: &str) -> usize {
    let refs = git_in(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/knives/"],
    )
    .unwrap_or_default();
    refs.lines()
        .map(|reference| {
            git_in(
                remote,
                &["grep", "-l", "-e", &format!("^{text}$"), reference],
            )
            .map_or(0, |files| files.lines().count())
        })
        .sum()
}

/// How many of the entries in `home`'s ledger of the lab fork read `text`.
fn recorded_in(home: &Path, text: &str) -> usize {
    Ledger::at(home.join("ledger").join(FORK))
        .entries()
        .expect("read the ledger")
        .iter()
        .filter(|entry| entry.text == text)
        .count()
}

/// Two machines sharing the remote at `alpha_remote` and `beta_remote`,
/// both of which have seen #7 open, so its merge is a transition to each.
fn two_machines_that_saw_seven_open(
    lab: &lab::Lab,
    alpha_remote: &Path,
    beta_remote: &Path,
) -> (tempfile::TempDir, tempfile::TempDir) {
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(lab);
    let beta = second_machine(alpha.path());
    share(alpha.path(), "alpha", alpha_remote);
    share(beta.path(), "beta", beta_remote);
    let open = forge_reporting("OPEN");
    for home in [&alpha, &beta] {
        let synced = sync_on(lab, home.path(), open.path());
        assert!(
            matches!(synced.status.code(), Some(0 | 3)),
            "{}\n{}",
            String::from_utf8_lossy(&synced.stdout),
            String::from_utf8_lossy(&synced.stderr)
        );
        assert_eq!(
            recorded_in(home.path(), MERGED),
            0,
            "a first sighting wrote an event"
        );
    }
    (alpha, beta)
}

/// Hold `home`'s ledger lock, as a running sweep does, until dropped.
fn hold_sweep_lock(home: &Path) -> File {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("ledger.lock"))
        .expect("open the ledger lock");
    file.lock().expect("take the ledger lock");
    file
}

#[test]
fn a_second_machine_syncing_the_same_merge_writes_no_second_event() {
    // Given: two machines sharing one ledger remote, both of which saw #7 open.
    let lab = lab::Lab::new();
    let (_remote_dir, remote) = bare_remote();
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &remote, &remote);

    // When: the forge reports #7 merged, alpha syncs and its sweep carries
    // the event, and then beta syncs with no sweep of its own able to run.
    let merged = forge_reporting("MERGED");
    let synced = sync_on(&lab, alpha.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    wait_until("alpha's event to reach the remote", || {
        recorded_on(&remote, MERGED) == 1
    });
    settle(alpha.path());
    let held = hold_sweep_lock(beta.path());
    let synced = sync_on(&lab, beta.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );

    // Then: beta's sync pulled alpha's event and wrote none of its own, so
    // no sweep had anything to discard.
    assert_eq!(recorded_in(beta.path(), MERGED), 1);

    // And: once beta sweeps, the remote has the merge once.
    drop(held);
    let swept = sweep(beta.path());
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    settle(beta.path());
    assert_eq!(recorded_in(beta.path(), MERGED), 1);
    assert_eq!(recorded_on(&remote, MERGED), 1);
    assert_eq!(entries_on(&remote, "beta"), 0);
}

#[test]
fn a_sweep_discards_its_own_uncommitted_repeat_of_a_transition_it_pulled() {
    // Given: two machines that saw #7 open, and beta's sweep held off, so
    // what beta writes stays uncommitted.
    let lab = lab::Lab::new();
    let (_remote_dir, remote) = bare_remote();
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &remote, &remote);
    let held = hold_sweep_lock(beta.path());

    // When: beta syncs the merge before alpha's event exists, then alpha
    // syncs it and its sweep carries it, and then beta sweeps.
    let merged = forge_reporting("MERGED");
    let synced = sync_on(&lab, beta.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    assert_eq!(recorded_in(beta.path(), MERGED), 1, "beta wrote its own");
    let synced = sync_on(&lab, alpha.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    wait_until("alpha's event to reach the remote", || {
        recorded_on(&remote, MERGED) == 1
    });
    settle(alpha.path());
    drop(held);
    let swept = sweep(beta.path());
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    settle(beta.path());

    // Then: beta's repeat is gone before any commit, it holds alpha's event
    // alone, and the remote has the merge once.
    assert_eq!(recorded_in(beta.path(), MERGED), 1);
    assert_eq!(recorded_on(&remote, MERGED), 1);
    assert_eq!(entries_on(&remote, "beta"), 0);
}

#[test]
fn status_names_the_repeated_transitions_a_handed_off_sweep_discarded() {
    // Given: two machines that saw #7 open, and beta's sweep held off while
    // it syncs the merge, so its event stays uncommitted.
    let lab = lab::Lab::new();
    let (_remote_dir, remote) = bare_remote();
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &remote, &remote);
    let held = hold_sweep_lock(beta.path());
    let merged = forge_reporting("MERGED");
    let synced = sync_on(&lab, beta.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    assert_eq!(recorded_in(beta.path(), MERGED), 1, "beta wrote its own");

    // When: alpha's event reaches the remote first, and then a note beta
    // writes hands off the sweep that discards beta's repeat, a sweep whose
    // output goes nowhere.
    let synced = sync_on(&lab, alpha.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    wait_until("alpha's event to reach the remote", || {
        recorded_on(&remote, MERGED) == 1
    });
    settle(alpha.path());
    drop(held);
    let wrote = knives_on(
        &lab,
        beta.path(),
        &["--text", "notch", "-m", "after the merge", "--repo", "demo"],
    );
    assert!(
        wrote.status.success(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    wait_until("beta's note to reach the remote", || {
        entries_on(&remote, "beta") == 1
    });
    settle(beta.path());
    assert_eq!(recorded_on(&remote, MERGED), 1);

    // Then: beta's status says its sweep discarded the repeat.
    let (code, report) = status_on(&lab, beta.path());
    assert_eq!(code, Some(0), "{report}");
    assert!(
        notes(&report)
            .iter()
            .any(|note| note.contains("discarded 1 repeated transition")),
        "{report}"
    );
}

#[test]
fn a_repeated_transition_two_machines_committed_is_never_removed() {
    // Given: two machines that saw #7 open, neither able to reach the remote
    // yet: each reaches it through a path that does not exist so far.
    let lab = lab::Lab::new();
    let (remote_dir, remote) = bare_remote();
    let alpha_view = remote_dir.path().join("alpha-view.git");
    let beta_view = remote_dir.path().join("beta-view.git");
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &alpha_view, &beta_view);

    // When: each syncs the merge, and its sweep commits the event to its own
    // ref but cannot push it.
    let merged = forge_reporting("MERGED");
    for (home, machine) in [(&alpha, "alpha"), (&beta, "beta")] {
        let _ = sync_on(&lab, home.path(), merged.path());
        let git_dir = home.path().join("ledger").join(".git");
        wait_until("the event to be committed", || {
            git_in(
                &git_dir,
                &["rev-parse", "--verify", &format!("refs/knives/{machine}")],
            )
            .is_some()
        });
        settle(home.path());
    }
    // And: the remote becomes reachable, and each machine sweeps until it
    // has the other's.
    std::os::unix::fs::symlink(&remote, &alpha_view).expect("link alpha's view");
    std::os::unix::fs::symlink(&remote, &beta_view).expect("link beta's view");
    for home in [&alpha, &beta, &alpha] {
        let swept = sweep(home.path());
        let stdout = String::from_utf8_lossy(&swept.stdout);
        assert!(swept.status.success(), "{stdout}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("a sweep report");
        assert_eq!(report["destinations"][0]["discarded"], 0, "{report}");
        settle(home.path());
    }

    // Then: both committed events stay, on each machine and on the remote.
    assert_eq!(recorded_in(alpha.path(), MERGED), 2);
    assert_eq!(recorded_in(beta.path(), MERGED), 2);
    assert_eq!(recorded_on(&remote, MERGED), 2);
}
