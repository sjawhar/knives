#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! `knives ledger sweep`, the send every write makes before it exits, and
//! the pull a deciding command makes first, through the real binary: a
//! config home whose registry shares a fork's ledger with a repository, and
//! whose ledger root is a git repository with that repository as its
//! `origin`, as a machine set up to share its ledger has.

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

fn append(root: &Path, fork: &str, count: usize) {
    let ledger = Ledger::at(root.join(fork));
    for index in 0..count {
        // Built from its serialized form, so it names no field a later
        // knives adds.
        let entry: Entry = serde_json::from_value(serde_json::json!({
            "ts": format!("2026-10-09T12:00:00.{index:09}Z"),
            "owner": "ses_sweep",
            "kind": Kind::Note,
            "text": format!("entry {index}"),
        }))
        .expect("build an entry");
        ledger.append(&entry).expect("append an entry");
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

/// Each entry file `machine`'s ref holds on `remote`, by its path under the
/// ledger root; none before it has one.
fn listed_on(remote: &Path, machine: &str) -> Vec<String> {
    git_in(
        remote,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            &format!("refs/knives/{machine}"),
        ],
    )
    .map_or_else(Vec::new, |listing| {
        listing
            .lines()
            .filter(|path| lab::is_entry(path))
            .map(str::to_owned)
            .collect()
    })
}

/// How many entry files `machine`'s ref holds on `remote`; `0` before it has one.
fn entries_on(remote: &Path, machine: &str) -> usize {
    listed_on(remote, machine).len()
}

/// Each entry file in `home`'s ledger of the lab fork, by its path under the
/// ledger root.
fn entries_in(home: &Path) -> Vec<String> {
    let directory = home.join("ledger").join(FORK);
    let mut names: Vec<String> = std::fs::read_dir(&directory)
        .map(|listing| {
            listing
                .map(|dirent| {
                    format!(
                        "{FORK}/{}",
                        dirent.expect("an entry").file_name().to_string_lossy()
                    )
                })
                .filter(|path| lab::is_entry(path))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
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

#[test]
fn a_burst_of_concurrent_sweeps_makes_one_commit_and_no_error() {
    // Given: twelve entries on a machine that shares its ledger.
    const SWEEPS: usize = 16;
    let home = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
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
    // And: each sweep, run by hand, waited for the one before it and ran its
    // own pass, so each reports that pass.
    let outcomes: Vec<String> = outputs
        .iter()
        .map(|output| {
            let report: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("a sweep reports JSON");
            report["outcome"].as_str().expect("an outcome").to_owned()
        })
        .collect();
    assert!(
        outcomes.iter().all(|outcome| outcome == "swept"),
        "{outcomes:?}"
    );
}

#[test]
fn a_write_that_finds_the_ledger_lock_held_waits_for_it_and_then_sends() {
    // Given: a managed fork on a machine that shares its ledger, and its
    // ledger lock held, as a running sweep or another write's send holds it.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(home.path(), "alpha", &remote);
    let held = hold_sweep_lock(home.path());

    // When: a note is written, and the holder lets go a moment later.
    let mut child = lab::knives_command(
        &lab.work,
        home.path(),
        lab.temp_path(),
        &[
            "--text",
            "notch",
            "-m",
            "after the holder",
            "--repo",
            "demo",
        ],
    )
    .env("KNIVES_OWNER", "ses_fff688")
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .expect("start knives notch");
    std::thread::sleep(Duration::from_secs(2));
    let early = child.try_wait().expect("look at the write");
    drop(held);
    let wrote = child.wait_with_output().expect("wait for the write");

    // Then: it waited for the holder rather than leave its note behind, and
    // the note is on the remote when it exits.
    let stderr = String::from_utf8_lossy(&wrote.stderr);
    assert!(
        early.is_none(),
        "the write exited under the held lock: {stderr}"
    );
    assert!(wrote.status.success(), "{stderr}");
    assert_eq!(entries_on(&remote, "alpha"), 1);
}

#[test]
fn a_sweep_run_by_hand_while_another_holds_the_lock_waits_and_reports_its_own_failed_push() {
    // Given: a machine with an entry to send, a ledger remote that refuses
    // every push, and its ledger lock held by a process that names itself
    // there, as a running sweep does.
    let home = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
    let hook = remote.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'push refused by the test' >&2\nexit 1\n",
    )
    .expect("write the pre-receive hook");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("make the hook executable");
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);
    let mut held = hold_sweep_lock(home.path());
    std::io::Write::write_all(&mut held, format!("{}\n", std::process::id()).as_bytes())
        .expect("name the holder");

    // When: a sweep is run by hand, and the holder lets go a moment later.
    let mut child = lab::knives_command(
        home.path(),
        home.path(),
        home.path(),
        &["--json", "ledger", "sweep"],
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .expect("start the sweep");
    std::thread::sleep(Duration::from_secs(2));
    let early = child.try_wait().expect("look at the sweep");
    drop(held);
    let swept = child.wait_with_output().expect("wait for the sweep");
    let stdout = String::from_utf8_lossy(&swept.stdout);

    // Then: it waited for the holder rather than leaving, ran its own pass,
    // and reports that pass: swept, naming the sweep it waited for, with the
    // refused push as a problem and exit 3.
    assert!(early.is_none(), "the sweep left at once: {stdout}");
    assert_eq!(swept.status.code(), Some(3), "{stdout}");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(report["outcome"], "swept", "{report}");
    assert_eq!(report["waited"]["pid"], std::process::id(), "{report}");
    assert!(
        problems_of(&report).contains("push refused by the test"),
        "{report}"
    );
    assert_eq!(entries_on(&remote, "alpha"), 0);
}

/// A bare remote whose every push is refused, with the directory holding it.
fn refusing_remote() -> (tempfile::TempDir, PathBuf) {
    let (remote_dir, remote) = lab::bare_remote();
    refuse_pushes(&remote);
    (remote_dir, remote)
}

/// Make `remote` refuse every push, saying so, until [`accept_pushes`].
fn refuse_pushes(remote: &Path) {
    remote_hook(
        remote,
        "#!/bin/sh\necho 'push refused by the test' >&2\nexit 1\n",
    );
}

/// Take back what [`refuse_pushes`] did.
fn accept_pushes(remote: &Path) {
    std::fs::remove_file(remote.join("hooks").join("pre-receive"))
        .expect("remove the pre-receive hook");
}

/// Run `script` as `remote`'s `pre-receive` hook, before every push.
fn remote_hook(remote: &Path, script: &str) {
    let hook = remote.join("hooks").join("pre-receive");
    std::fs::write(&hook, script).expect("write the pre-receive hook");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("make the hook executable");
}

#[test]
fn a_sweep_whose_push_is_refused_counts_the_entries_it_leaves_unsent() {
    // Given: a machine with two entries to send, and a ledger remote that
    // refuses every push.
    let home = a_repo_home();
    let (_remote_dir, remote) = refusing_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 2);

    // When: a sweep runs.
    let (code, report) = sweep_report(home.path());

    // Then: it fails, and says both entries are still on this machine alone.
    assert_eq!(code, Some(3), "{report}");
    assert_eq!(report["destinations"][0]["unsent"], 2, "{report}");
    assert_eq!(entries_on(&remote, "alpha"), 0);
}

#[test]
fn a_new_machine_whose_remote_cannot_be_reached_has_nothing_unsent() {
    // Given: a machine joined as the installer joins one, with no entries
    // and a ledger remote that cannot be reached.
    let home = a_repo_home();
    join_as_the_installer_does(home.path(), "alpha", Path::new("/nonexistent/ledger.git"));

    // When: a sweep runs.
    let (code, report) = sweep_report(home.path());

    // Then: the failed fetch is a problem, and nothing is left unsent.
    assert_eq!(code, Some(3), "{report}");
    assert!(
        problems_of(&report).contains("/nonexistent/ledger.git"),
        "{report}"
    );
    assert_eq!(report["destinations"][0]["unsent"], 0, "{report}");
}

#[test]
fn a_sweep_that_sends_everything_has_nothing_unsent() {
    // Given: a machine with an entry to send and a remote that takes it.
    let home = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);

    // When: a sweep runs.
    let (code, report) = sweep_report(home.path());

    // Then: it sent it, and says nothing is left.
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["destinations"][0]["unsent"], 0, "{report}");
    assert_eq!(entries_on(&remote, "alpha"), 1);
}

#[test]
fn a_sweep_with_nothing_new_does_nothing_and_succeeds() {
    // Given: a machine whose one entry is already swept.
    let home = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
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
    let (_remote_dir, remote) = lab::bare_remote();
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
            lab::with_git_rule(
                &mut command,
                "include.path",
                include.to_str().expect("utf-8 path"),
            );
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
fn a_fork_whose_ledger_no_git_directory_reaches_is_joined_and_its_sweep_names_that_ledger() {
    // Given: a registry sharing a fork's ledger with one repository, a root
    // whose origin is another, an entry of that fork's on disk, and the
    // ledger's own remote, which git reaches only through the URL rule the
    // sweep's session hands it, and which refuses every push.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
         origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"acme/elsewhere\"\n",
    )
    .expect("write the registry");
    let (_remote_dir, remote) = lab::bare_remote();
    let root = share(home.path(), "alpha", &remote);
    append(&root, "acme/a-repo", 1);
    let (_elsewhere_dir, elsewhere) = refusing_remote();

    // When: a sweep runs.
    let swept = lab::with_git_rule(
        &mut lab::knives_command(
            home.path(),
            home.path(),
            home.path(),
            &["--json", "ledger", "sweep"],
        ),
        &format!("url.{}.insteadOf", elsewhere.display()),
        "https://forge.invalid/acme/elsewhere",
    )
    .output()
    .expect("run knives ledger sweep");

    // Then: it joined the ledger the registry names, with a git directory
    // over the root whose origin is that ledger on the fork's forge and
    // which names this machine, and said so in one line.
    let joined = home
        .path()
        .join("ledger-repositories")
        .join("acme--elsewhere.git");
    let config = |key: &str| git_in(&joined, &["config", "--get", key]);
    let machine = config("knives.machine").expect("the joined git directory names a machine");
    let stderr = String::from_utf8_lossy(&swept.stderr);
    assert!(
        stderr.contains(&format!(
            "joined the ledger acme/elsewhere: made {} as machine {machine}, carrying \
             acme/a-repo\n",
            joined.display()
        )),
        "{stderr}"
    );
    assert_eq!(
        config("remote.origin.url").as_deref(),
        Some("https://forge.invalid/acme/elsewhere")
    );
    assert_eq!(config("core.bare").as_deref(), Some("false"));
    assert_eq!(config("core.worktree"), Some(root.display().to_string()));
    // And: the sweep carries the fork through that directory alone, and
    // fails there, since the ledger refuses the push, naming the directory
    // (`acme--elsewhere.git`, the ledger's) and git's cause; it carries the
    // entry to the root's remote no more than before, and leaves the reason
    // in the log.
    assert_eq!(swept.status.code(), Some(3), "{swept:?}");
    let report: serde_json::Value = serde_json::from_slice(&swept.stdout).expect("JSON");
    let carrying: Vec<&serde_json::Value> = report["destinations"]
        .as_array()
        .expect("destinations")
        .iter()
        .filter(|destination| destination["forks"] != serde_json::json!([]))
        .collect();
    assert!(
        matches!(carrying.as_slice(), [destination]
            if destination["git_dir"] == joined.display().to_string()
                && destination["forks"] == serde_json::json!(["acme/a-repo"])),
        "{report}"
    );
    let problems = report["problems"].to_string();
    let through = format!("origin ({})", joined.display());
    assert!(
        problems.contains(&through) && problems.contains("push refused by the test"),
        "{report}"
    );
    assert_eq!(entries_on(&remote, "alpha"), 0);
    let log = std::fs::read_to_string(home.path().join("ledger-sweep.log")).expect("a log");
    assert!(log.contains(&through), "{log}");
}

/// What [`sweep_raced_by_a_join`] left.
struct Raced {
    swept: Output,
    /// `acme--zzz.git` in the config home's `ledger-repositories`: the git
    /// directory joining `acme/zzz`, which the other command put there.
    git_dir: PathBuf,
    /// The config home, the remotes and the other command's directory.
    _kept: Vec<tempfile::TempDir>,
}

/// A sweep that another command races to join `acme/zzz`.
///
/// The config home's registry shares `a-repo`'s ledger with `acme/aaa` and
/// `z-repo`'s with `acme/zzz`. A git directory joins `acme/aaa` already; no
/// git directory reaches `acme/zzz`. A `git` on the sweep's `PATH` renames a
/// whole git directory, whose origin is `staged_origin` and whose machine is
/// `m-staged`, into place as `acme--zzz.git` the first time the sweep runs
/// git on `acme--aaa.git`. That is after the sweep lists the git directories
/// and before it joins `acme/zzz`.
fn sweep_raced_by_a_join(staged_origin: &str) -> Raced {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
         origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"acme/aaa\"\n\n\
         [repos.z-repo]\nupstream = \"https://forge.invalid/acme/z-repo\"\n\
         origin = \"https://forge.invalid/ours/z-repo\"\nledger = \"acme/zzz\"\n",
    )
    .expect("write the registry");
    let root = home.path().join("ledger");
    let repositories = home.path().join("ledger-repositories");
    let (aaa_dir, aaa) = lab::bare_remote();
    lab::join_ledger(
        &repositories.join("acme--aaa.git"),
        &root,
        "https://forge.invalid/acme/aaa",
        &aaa,
        "m-aaa",
    );
    let (staged_dir, staged_remote) = lab::bare_remote();
    let racer = tempfile::tempdir().expect("the other command's directory");
    let staged = racer.path().join("acme--zzz.git");
    lab::join_ledger(&staged, &root, staged_origin, &staged_remote, "m-staged");
    let git_dir = repositories.join("acme--zzz.git");
    let path = std::env::var_os("PATH").expect("PATH");
    let real = std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    let bin = racer.path().join("bin");
    std::fs::create_dir_all(&bin).expect("the racing git's directory");
    let script = bin.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             case \" $* \" in\n\
             *acme--aaa.git*)\n\
             \x20 if [ -d '{staged}' ] && [ ! -e '{git_dir}' ]; then mv '{staged}' '{git_dir}'; fi;;\n\
             esac\n\
             exec '{real}' \"$@\"\n",
            staged = staged.display(),
            git_dir = git_dir.display(),
            real = real.display()
        ),
    )
    .expect("write the racing git");
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("make the racing git executable");
    let swept = lab::knives_command(
        home.path(),
        home.path(),
        home.path(),
        &["--json", "ledger", "sweep"],
    )
    .env(
        "PATH",
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path)))
            .expect("join PATH"),
    )
    .output()
    .expect("run knives ledger sweep");
    assert!(
        !staged.exists(),
        "the racing git renamed nothing: {swept:?}"
    );
    Raced {
        swept,
        git_dir,
        _kept: vec![home, aaa_dir, staged_dir, racer],
    }
}

#[test]
fn a_ledger_another_command_joins_between_the_listing_and_the_join_carries_its_fork() {
    // Given: a config home whose `acme/zzz` ledger no git directory reaches,
    // and another command that puts a git directory whose origin is
    // `acme/zzz` in place after the sweep lists the git directories and
    // before it joins that ledger.
    // When: a sweep runs.
    let raced = sweep_raced_by_a_join("https://forge.invalid/acme/zzz");

    // Then: it succeeds, and says of no git directory that it is there
    // already.
    let stdout = String::from_utf8_lossy(&raced.swept.stdout);
    let stderr = String::from_utf8_lossy(&raced.swept.stderr);
    assert_eq!(raced.swept.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(
        !stdout.contains("is there already") && !stderr.contains("is there already"),
        "{stdout}\n{stderr}"
    );
    // And: it carries `z-repo` through the git directory the other command
    // made, which still names that command's machine.
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    assert!(
        report["destinations"]
            .as_array()
            .expect("destinations")
            .iter()
            .any(
                |destination| destination["git_dir"] == raced.git_dir.display().to_string()
                    && destination["forks"] == serde_json::json!(["acme/z-repo"])
            ),
        "{report}"
    );
    assert_eq!(
        git_in(&raced.git_dir, &["config", "--get", "knives.machine"]).as_deref(),
        Some("m-staged")
    );
}

#[test]
fn a_git_directory_another_command_puts_in_place_with_another_origin_is_there_already() {
    // Given: a config home whose `acme/zzz` ledger no git directory reaches,
    // and another command that puts a git directory whose origin is
    // `acme/other` where joining `acme/zzz` makes one, after the sweep lists
    // the git directories and before it joins that ledger.
    // When: a sweep runs.
    let raced = sweep_raced_by_a_join("https://forge.invalid/acme/other");

    // Then: it fails, saying that git directory is there already and its
    // origin is not `acme/zzz`, and leaves that git directory as it was.
    let stdout = String::from_utf8_lossy(&raced.swept.stdout);
    let stderr = String::from_utf8_lossy(&raced.swept.stderr);
    assert_eq!(raced.swept.status.code(), Some(3), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!(
            "{} is there already, and its origin is not acme/zzz",
            raced.git_dir.display()
        )),
        "{stdout}\n{stderr}"
    );
    assert_eq!(
        git_in(&raced.git_dir, &["config", "--get", "remote.origin.url"]).as_deref(),
        Some("https://forge.invalid/acme/other")
    );
}

#[test]
fn a_ledger_spelled_with_capitals_or_an_uppercase_git_suffix_reaches_its_repository() {
    for spelling in ["Acme/Ledger", "acme/ledger.GIT", "ACME/LEDGER.Git"] {
        // Given: a registry naming the ledger repository with that
        // spelling, a root whose origin is that repository, and an entry.
        let home = tempfile::tempdir().expect("config home");
        std::fs::write(
            home.path().join("repos.toml"),
            format!(
                "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
                 origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"{spelling}\"\n"
            ),
        )
        .expect("write the registry");
        let (_remote_dir, remote) = lab::bare_remote();
        let root = share(home.path(), "alpha", &remote);
        append(&root, "acme/a-repo", 1);

        // When: a sweep runs.
        let swept = sweep(home.path());

        // Then: the fork is carried there, and its entry reached it.
        let stdout = String::from_utf8_lossy(&swept.stdout);
        assert!(swept.status.success(), "{spelling}: {stdout}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
        assert_eq!(
            report["destinations"][0]["forks"],
            serde_json::json!(["acme/a-repo"]),
            "{spelling}: {report}"
        );
        assert_eq!(entries_on(&remote, "alpha"), 1, "{spelling}");
    }
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
    let (_company_dir, company) = lab::bare_remote();
    let (_personal_dir, personal) = lab::bare_remote();
    let root = share(home.path(), "alpha", &company);
    let personal_url = "https://forge.invalid/someone/.knives-ledger";
    lab::join_ledger(
        &home.path().join("ledger-repositories").join("personal.git"),
        &root,
        personal_url,
        &personal,
        "alpha",
    );
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
        let entries: Vec<&str> = listing.lines().filter(|path| lab::is_entry(path)).collect();
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
fn a_write_on_a_fork_with_no_ledger_exits_0_and_makes_no_network_call() {
    // Given: a managed fork whose registry entry names no ledger.
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);

    // When: a note is written.
    let wrote = knives_on(
        &lab,
        home.path(),
        &["--text", "notch", "-m", "kept here", "--repo", "demo"],
    );

    // Then: it succeeded and said nothing on stderr, and nothing was set up
    // to carry the note anywhere: no git directory over the ledger, none
    // joined, and the ledger's lock never taken.
    assert!(
        wrote.status.success() && wrote.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    assert!(home.path().join("ledger").is_dir());
    assert!(!home.path().join("ledger").join(".git").exists());
    assert!(!home.path().join("ledger-repositories").exists());
    assert!(!home.path().join("ledger.lock").exists());
    assert!(!home.path().join("ledger-sweep.log").exists());
}

#[test]
fn a_burst_of_concurrent_writes_reaches_the_remote_without_a_failure() {
    // Given: a managed fork on a machine that shares its ledger.
    const WRITES: usize = 16;
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(home.path(), "alpha", &remote);

    // When: sixteen notes are written at once, each sending its own.
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

    // Then: every write succeeds and says nothing on stderr.
    for child in children {
        let output = child.wait_with_output().expect("wait for a write");
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "a write failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // And: when the last one has exited, every note is on the remote, and
    // no send left a failure behind.
    assert_eq!(entries_on(&remote, "alpha"), WRITES);
    assert!(!home.path().join("ledger-sweep.log").exists());
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
    let (_remote_dir, remote) = lab::bare_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);

    // When: alpha states the branch's pull request, which is on the remote
    // when the write exits.
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
    assert_eq!(entries_on(&remote, "alpha"), 1);
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
    lab::join_ledger(
        &home.join("ledger-repositories").join("acme--ledger.git"),
        &home.join("ledger"),
        LEDGER_URL,
        remote,
        machine,
    );
}

#[test]
fn a_new_machine_with_no_ledger_root_yet_pulls_what_another_stated() {
    // Given: alpha's statement on the remote, and beta joined as the
    // installer joins a new machine, with no ledger root at all.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
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
    assert_eq!(entries_on(&remote, "alpha"), 1);
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
fn a_status_with_nothing_to_pull_leaves_a_missing_ledger_root_missing() {
    // Given: a machine joined as the installer joins one, with no ledger
    // root, sharing with a remote that carries nothing yet.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
    join_as_the_installer_does(beta.path(), "beta", &remote);
    assert!(!beta.path().join("ledger").exists());

    // When: beta's status runs, pulling first.
    let (code, report) = status_on(&lab, beta.path());

    // Then: it answers with no problem, and wrote no ledger root: a read
    // creates nothing, and a pull with nothing to write writes nothing.
    assert_eq!(code, Some(0), "{report}");
    assert!(report.get("problems").is_none(), "{report}");
    assert!(!beta.path().join("ledger").exists());
}

#[test]
fn a_finish_run_from_inside_the_workspace_it_removes_still_sends_its_release() {
    // Given: a machine joined as the installer joins one, and a branch it
    // started into a workspace, whose claim is on the remote.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    join_as_the_installer_does(home.path(), "alpha", &remote);
    let started = knives_on(
        &lab,
        home.path(),
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
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let workspace = lab.temp_path().join("feat-gamma");
    assert!(
        workspace.is_dir(),
        "no workspace at {}",
        workspace.display()
    );
    assert_eq!(entries_on(&remote, "alpha"), 1);

    // When: the branch is finished from inside that workspace, which the
    // finish removes before it sends its release.
    let finished = lab::knives_command(
        &workspace,
        home.path(),
        lab.temp_path(),
        &["--text", "finish", "feat/gamma", "--repo", "demo"],
    )
    .env("KNIVES_OWNER", "ses_fff688")
    .output()
    .expect("run knives finish");
    assert!(
        finished.status.success(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    assert!(!workspace.exists(), "the workspace survived the finish");

    // Then: the release is on the remote when the finish exits, and no
    // failure is left behind.
    let log = home.path().join("ledger-sweep.log");
    assert!(
        !log.exists(),
        "the send failed: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert_eq!(recorded_on(&remote, "claim released"), 1);
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
    let (_remote_dir, remote) = lab::bare_remote();
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
    assert_eq!(entries_on(&remote, "alpha"), 1);

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

    // Then: beta's statement, the newest, lists both.
    let entries = Ledger::at(beta.path().join("ledger").join(FORK))
        .read()
        .expect("read beta's ledger")
        .entries;
    let live = knives::statement::Statements::from_entries(&entries);
    assert_eq!(live.depends("feat/alpha"), ["acme/demo#1", "acme/demo#2"]);
}

#[test]
fn dependencies_two_machines_state_at_once_both_survive() {
    // Given: two machines sharing one ledger remote, which refuses every
    // push for now, so neither has the other's statement when it states its
    // own.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = refusing_remote();
    share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);

    // When: each states a different dependency of the same branch, which
    // stays on its machine, then the remote takes pushes again, and each
    // sweeps until it has the other's.
    for (home, on) in [(&alpha, "demo#1"), (&beta, "demo#2")] {
        let stated = knives_on(
            &lab,
            home.path(),
            &["--text", "depends", "feat/alpha", "--on", on],
        );
        let stderr = String::from_utf8_lossy(&stated.stderr);
        assert_eq!(stated.status.code(), Some(3), "{stderr}");
        assert!(
            stderr.contains("1 ledger entry this command wrote did not reach the ledger"),
            "{stderr}"
        );
    }
    accept_pushes(&remote);
    for home in [&alpha, &beta, &alpha] {
        let swept = sweep(home.path());
        assert!(
            swept.status.success(),
            "{}",
            String::from_utf8_lossy(&swept.stdout)
        );
    }

    // Then: both dependencies are live on both machines.
    for home in [&alpha, &beta] {
        let entries = Ledger::at(home.path().join("ledger").join(FORK))
            .read()
            .expect("read the ledger")
            .entries;
        let live = knives::statement::Statements::from_entries(&entries);
        assert_eq!(
            live.depends("feat/alpha"),
            ["acme/demo#1", "acme/demo#2"],
            "{entries:?}"
        );
    }
}

#[test]
fn a_track_from_a_machine_whose_clock_runs_behind_wins_over_the_statement_it_replaced() {
    // Given: two machines sharing one ledger remote, and alpha's statement of
    // #12 on it, stamped five minutes after beta's clock, as alpha stamps it
    // when beta's clock runs five minutes behind alpha's.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
    let alpha_root = share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let ahead = jiff::Timestamp::now() + jiff::SignedDuration::from_mins(5);
    // Built from its serialized form, so it names no field a later knives
    // adds.
    let statement = knives::statement::Statement {
        kind: knives::statement::StatementKind::Pull,
        value: Some("12".to_owned()),
    };
    let stated: Entry = serde_json::from_value(serde_json::json!({
        "ts": ahead.to_string(),
        "owner": "ses_alpha",
        "subject": "feat/alpha",
        "kind": Kind::Event,
        "statement": statement,
        "text": "stated as #12",
        "pr": 12,
    }))
    .expect("build alpha's statement");
    Ledger::at(alpha_root.join(FORK))
        .append(&stated)
        .expect("append alpha's statement");
    assert!(sweep(alpha.path()).status.success());
    let pr_on = |home: &Path| {
        let (code, report) = status_on(&lab, home);
        assert_eq!(code, Some(0), "{report}");
        let row = report["branches"]
            .as_array()
            .expect("branch rows")
            .iter()
            .find(|row| row["name"] == "feat/alpha")
            .unwrap_or_else(|| panic!("no feat/alpha row: {report}"))
            .clone();
        row["pr"]["number"].clone()
    };
    assert_eq!(pr_on(beta.path()), 12, "beta did not pull alpha's #12");

    // When: beta states #13, replacing the #12 it read, which is on the
    // remote when the write exits.
    let tracked = knives_on(
        &lab,
        beta.path(),
        &["--text", "track", "feat/alpha", "--pr", "13"],
    );
    assert!(
        tracked.status.success(),
        "{}",
        String::from_utf8_lossy(&tracked.stderr)
    );
    assert_eq!(entries_on(&remote, "beta"), 1);

    // Then: #13 is live on beta, and on alpha once it pulls.
    assert_eq!(pr_on(beta.path()), 13);
    assert_eq!(pr_on(alpha.path()), 13);
}

#[test]
fn a_forget_with_nothing_stated_leaves_a_peers_unsent_statement_standing() {
    // Given: two machines sharing one ledger remote, and a pull request
    // alpha stated while its pushes went to a repository that does not
    // exist, so the statement stayed on alpha.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let alpha = shared_home(&lab);
    let beta = second_machine(alpha.path());
    let (_remote_dir, remote) = lab::bare_remote();
    let alpha_root = share(alpha.path(), "alpha", &remote);
    share(beta.path(), "beta", &remote);
    let nowhere = "url./nonexistent/ledger.git.pushInsteadOf";
    lab::git_output(&alpha_root, ["config", nowhere, LEDGER_URL]);
    let tracked = knives_on(
        &lab,
        alpha.path(),
        &["--text", "track", "feat/alpha", "--pr", "1234"],
    );
    assert_eq!(
        tracked.status.code(),
        Some(3),
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
    lab::git_output(&alpha_root, ["config", "--unset", nowhere]);
    assert!(sweep(alpha.path()).status.success());

    // Then: beta wrote no forget, so once it pulls, alpha's #1234 is live
    // on beta too.
    let entries = Ledger::at(beta.path().join("ledger").join(FORK))
        .read()
        .expect("read beta's ledger")
        .entries;
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
                .read()
                .expect("read the ledger")
                .entries
                .is_empty(),
            "{args:?} wrote an entry"
        );
    }

    // And: stating a pull request, which replaces nothing it read, still
    // records, and exits 3 naming the ledger its entry did not reach.
    let tracked = knives_on(
        &lab,
        home.path(),
        &["--text", "track", "feat/alpha", "--pr", "7"],
    );
    let stderr = String::from_utf8_lossy(&tracked.stderr);
    assert_eq!(tracked.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}: 1 ledger entry this command wrote did not reach the ledger {LEDGER}"
        )),
        "{stderr}"
    );
    assert_eq!(entries_in(home.path()).len(), 1);
}

#[test]
fn a_write_whose_ledger_remote_stalls_exits_3_within_a_bound_naming_the_ledger() {
    // Given: a managed fork on a machine whose ledger remote accepts the
    // connection and never answers.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let root = share_at(home.path(), "alpha", &stalled_remote());

    // When: a note is written.
    let started = Instant::now();
    let wrote = knives_on(
        &lab,
        home.path(),
        &["--text", "notch", "-m", "stalled", "--repo", "demo"],
    );

    // Then: it exits 3 well inside a minute, naming the ledger and the git
    // directory its note did not reach, records why in the sweep log, and
    // no longer holds the ledger's lock.
    let stderr = String::from_utf8_lossy(&wrote.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the write hung for {:?}",
        started.elapsed()
    );
    assert_eq!(wrote.status.code(), Some(3), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "{FORK}: 1 ledger entry this command wrote did not reach the ledger {LEDGER} ({}): ",
            root.join(".git").display()
        )),
        "{stderr}"
    );
    let recorded =
        std::fs::read_to_string(home.path().join("ledger-sweep.log")).expect("read the sweep log");
    assert!(recorded.contains("origin"), "{recorded}");
    assert!(!held(&home.path().join("ledger.lock")));
}

#[test]
fn status_counts_entries_the_remote_lacks_and_names_a_failed_sweeps_log() {
    // Given: a shared ledger with one entry no sweep has carried.
    let lab = lab::Lab::new();
    let home = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
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
    let (_remote_dir, remote) = lab::bare_remote();
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
    assert_eq!(entries_on(&remote, "alpha"), 1);

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
    // it is not read.
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
    let (_remote_dir, remote) = lab::bare_remote();
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
    let (_remote_dir, remote) = lab::bare_remote();
    let root = share(home.path(), "alpha", &remote);
    let cut = lab::knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
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
    let (_remote_dir, remote) = lab::bare_remote();
    let root = share(home.path(), "alpha", &remote);
    let cut = lab::knives_release(&lab, &home, &["cut", "release/2026-08-04"]);
    assert!(
        cut.status.success(),
        "{}",
        String::from_utf8_lossy(&cut.stdout)
    );
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
    let (_remote_dir, remote) = lab::bare_remote();
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
    let (_remote_dir, remote) = lab::bare_remote();
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
    let (_remote_dir, remote) = lab::bare_remote();
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
fn a_non_fast_forward_push_names_a_machine_name_collision() {
    // Given: two config homes whose separate ledger repositories use the same
    // machine name and each have an entry to commit.
    let first = a_repo_home();
    let second = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
    let first_root = share(first.path(), "duplicate", &remote);
    let second_root = share(second.path(), "duplicate", &remote);
    append(&first_root, "acme/a-repo", 1);
    append(&second_root, "acme/a-repo", 1);
    let hook = remote.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         waiting=\"$GIT_DIR/first-push-waiting\"\n\
         release=\"$GIT_DIR/release-first-push\"\n\
         if [ ! -e \"$waiting\" ]; then\n\
           : > \"$waiting\"\n\
           while [ ! -e \"$release\" ]; do sleep 0.01; done\n\
         fi\n",
    )
    .expect("write the pre-receive hook");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("make the pre-receive hook executable");

    // When: the second machine commits, but its push pauses before the remote
    // receives it. The first therefore commits and pushes an unrelated history
    // under the same ref before the second push is released.
    let second_sweep = lab::knives_command(
        second.path(),
        second.path(),
        second.path(),
        &["--json", "ledger", "sweep"],
    )
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .expect("start the second sweep");
    wait_until("the second push to wait at the remote", || {
        remote.join("first-push-waiting").exists()
    });
    assert!(
        git_in(
            &second_root.join(".git"),
            &["rev-parse", "--verify", "refs/knives/duplicate"],
        )
        .is_some(),
        "the second sweep did not commit before waiting to push"
    );
    let first_swept = sweep(first.path());
    assert!(
        first_swept.status.success(),
        "{}",
        String::from_utf8_lossy(&first_swept.stdout)
    );
    std::fs::write(remote.join("release-first-push"), "").expect("release the second push");
    let second_swept = second_sweep
        .wait_with_output()
        .expect("wait for the second sweep");

    // Then: the second sweep identifies another machine using its name and
    // tells the operator how to repair that configuration collision.
    assert_eq!(
        second_swept.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&second_swept.stdout)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&second_swept.stdout).expect("a sweep report");
    let problems = problems_of(&report);
    assert!(
        problems.contains("another machine may be committing under the same name")
            && problems.contains("give each machine a unique knives.machine"),
        "{report}"
    );
}

#[test]
fn an_entry_a_name_collision_left_unsent_reaches_the_remote_after_the_rename_it_names() {
    // Given: two config homes committing as one machine name, `duplicate`.
    // The first pushed its ref; the second took that ref up, as a join of
    // the same session takes up its own ref, and pushed on top of it.
    let first = a_repo_home();
    let second = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
    let first_root = share(first.path(), "duplicate", &remote);
    let second_root = share(second.path(), "duplicate", &remote);
    append(&first_root, "acme/a-repo", 1);
    let (code, report) = sweep_report(first.path());
    assert_eq!(code, Some(0), "{report}");
    lab::git_output(
        &second_root,
        [
            "fetch",
            "--quiet",
            remote.to_str().expect("utf-8"),
            "refs/knives/duplicate:refs/knives/duplicate",
        ],
    );
    append(&second_root, "acme/a-repo", 1);
    let (code, report) = sweep_report(second.path());
    assert_eq!(code, Some(0), "{report}");

    // When: the first writes again, its push is refused as a name
    // collision, and the first is renamed as that refusal says.
    let entries = |root: &Path| -> std::collections::BTreeSet<String> {
        std::fs::read_dir(root.join("acme/a-repo"))
            .expect("read the fork's entries")
            .map(|dirent| {
                format!(
                    "acme/a-repo/{}",
                    dirent.expect("an entry").file_name().to_string_lossy()
                )
            })
            .filter(|path| lab::is_entry(path))
            .collect()
    };
    let before = entries(&first_root);
    append(&first_root, "acme/a-repo", 1);
    let refused: Vec<String> = entries(&first_root).difference(&before).cloned().collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
    let (code, report) = sweep_report(first.path());
    assert_eq!(code, Some(3), "{report}");
    assert!(
        problems_of(&report).contains("give each machine a unique knives.machine"),
        "{report}"
    );
    lab::git_output(&first_root, ["config", "knives.machine", "renamed"]);
    let (code, report) = sweep_report(first.path());

    // Then: that sweep sends the entry the refused push left, under the
    // new name, and leaves nothing unsent: a write that did not reach the
    // ledger can be sent again, by the remedy knives itself gives.
    assert_eq!(code, Some(0), "{report}");
    assert_eq!(report["destinations"][0]["unsent"], 0, "{report}");
    let on_renamed = listed_on(&remote, "renamed");
    assert!(
        on_renamed.contains(&refused[0]),
        "{} is not on the remote's renamed ref: {on_renamed:?}",
        refused[0]
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
    let (_remote_dir, remote) = lab::bare_remote();
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
            .read()
            .expect("beta's ledger")
            .entries
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

#[test]
fn a_peer_directory_named_with_control_characters_is_refused_and_reaches_the_terminal_escaped() {
    // Given: a machine carrying one fork, and a peer's ref on the remote
    // carrying an entry, named as knives names one, under an owner
    // directory whose name holds an OSC title-set sequence: a directory no
    // fork can be kept under.
    let home = a_repo_home();
    let (_remote_dir, remote) = lab::bare_remote();
    share(home.path(), "alpha", &remote);
    let owner = "\u{1b}]0;pwned\u{7}";
    lab::push_as_peer(
        &remote,
        "mallory",
        &format!("{owner}/repo/20261011T120000.000000000Z-0000.md"),
        &peer_entry("from a peer"),
    );

    // When: a sweep runs and renders for a person.
    let swept = lab::knives_command(
        home.path(),
        home.path(),
        home.path(),
        &["--text", "ledger", "sweep"],
    )
    .output()
    .expect("run knives ledger sweep");

    // Then: it refuses the peer's tree, writes nothing from it, and says
    // so with no control character in what it prints.
    let stdout = String::from_utf8_lossy(&swept.stdout);
    assert_eq!(swept.status.code(), Some(3), "{stdout:?}");
    assert!(stdout.contains("which is not a ledger entry"), "{stdout:?}");
    assert!(!stdout.contains("left unread"), "{stdout:?}");
    assert!(
        !stdout.contains('\u{1b}') && !stdout.contains('\u{7}'),
        "{stdout:?}"
    );
    assert!(!home.path().join("ledger").join(owner).exists());
}

/// The problems of a report, as one string to search.
fn problems_of(report: &serde_json::Value) -> String {
    report["problems"].to_string()
}

#[test]
fn a_peer_entry_of_a_kind_this_knives_does_not_know_leaves_notch_and_status_readable() {
    // Given: a machine sharing the lab fork's ledger, and a peer's ref on
    // the remote carrying an entry of a kind a newer knives writes.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let beta = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(beta.path(), "beta", &remote);
    let name = "20261011T120000.000000000Z-0000.md";
    lab::push_as_peer(
        &remote,
        "alpha",
        &format!("{FORK}/{name}"),
        "+++\nts = \"2026-10-11T12:00:00Z\"\nowner = \"ses_peer\"\nsubject = \"feat/alpha\"\n\
         kind = \"decision\"\n+++\nwe decided\n",
    );
    let on_disk = beta.path().join("ledger").join(FORK).join(name);

    for round in ["the first pull", "a pull after the file was deleted"] {
        // When: status pulls, then notch reads.
        let (code, report) = status_on(&lab, beta.path());

        // Then: status answers, with its branches, naming the entry as a
        // problem.
        assert_eq!(code, Some(3), "{round}: {report}");
        assert!(problems_of(&report).contains(name), "{round}: {report}");
        assert!(
            report["branches"]
                .as_array()
                .expect("branch rows")
                .iter()
                .any(|row| row["name"] == "feat/alpha"),
            "{round}: {report}"
        );
        assert!(on_disk.exists(), "{round}: the pull did not write it");
        // And: notch reads too, naming it.
        let notch = knives_on(&lab, beta.path(), &["--json", "notch"]);
        let read: serde_json::Value = serde_json::from_slice(&notch.stdout).unwrap_or_else(|_| {
            panic!(
                "{round}: notch emits JSON: {}",
                String::from_utf8_lossy(&notch.stderr)
            )
        });
        assert_eq!(notch.status.code(), Some(3), "{round}: {read}");
        assert!(problems_of(&read).contains(name), "{round}: {read}");

        // And: deleting the local copy is no way out; the next pull writes
        // it back.
        std::fs::remove_file(&on_disk).expect("delete the local copy");
    }
}

#[test]
fn a_peer_entry_that_does_not_parse_is_kept_out_and_named_on_every_pull() {
    // Given: a machine sharing the lab fork's ledger, and a peer's ref on
    // the remote carrying a file no knives can read as an entry.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let beta = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(beta.path(), "beta", &remote);
    let path = format!("{FORK}/20261011T120000.000000000Z-dead.md");
    lab::push_as_peer(&remote, "alpha", &path, "not an entry\n");

    for round in ["the first pull", "the next pull"] {
        // When: status pulls.
        let (code, report) = status_on(&lab, beta.path());

        // Then: it answers, naming the peer and the file, and wrote nothing.
        assert_eq!(code, Some(3), "{round}: {report}");
        let problems = problems_of(&report);
        assert!(
            problems.contains(&path) && problems.contains("alpha"),
            "{round}: {report}"
        );
        assert!(
            !beta.path().join("ledger").join(&path).exists(),
            "{round}: the unreadable entry was written"
        );
        // And: the ledger this machine has still reads.
        let notch = knives_on(&lab, beta.path(), &["--json", "notch"]);
        assert_eq!(
            notch.status.code(),
            Some(0),
            "{round}: {}",
            String::from_utf8_lossy(&notch.stderr)
        );
    }
}

/// A valid entry about the lab fork's `feat/alpha` reading `text`, as a peer
/// would send it.
fn peer_entry(text: &str) -> String {
    format!(
        "+++\nts = \"2026-10-11T12:00:00Z\"\nowner = \"ses_peer\"\nsubject = \"feat/alpha\"\n\
         kind = \"note\"\n+++\n{text}\n"
    )
}

/// `home`'s sweep report, with its exit code.
fn sweep_report(home: &Path) -> (Option<i32>, serde_json::Value) {
    let swept = sweep(home);
    let report = serde_json::from_slice(&swept.stdout).unwrap_or_else(|error| {
        panic!(
            "the sweep emits JSON ({error}): {}",
            String::from_utf8_lossy(&swept.stderr)
        )
    });
    (swept.status.code(), report)
}

#[test]
fn two_peers_carrying_different_entries_at_one_path_write_neither_and_say_so() {
    // Given: two peers' refs carry different entries under one file name.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let beta = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(beta.path(), "beta", &remote);
    let path = format!("{FORK}/20261011T120000.000000000Z-0000.md");
    lab::push_as_peer(&remote, "alpha", &path, &peer_entry("alpha's account"));
    lab::push_as_peer(&remote, "gamma", &path, &peer_entry("gamma's account"));
    let on_disk = beta.path().join("ledger").join(&path);

    // When: status pulls.
    let (code, report) = status_on(&lab, beta.path());

    // Then: neither entry is written, and the problem names the path and
    // both refs that carry it.
    assert_eq!(code, Some(3), "{report}");
    let problems = problems_of(&report);
    for named in [path.as_str(), "refs/knives/alpha", "refs/knives/gamma"] {
        assert!(problems.contains(named), "{named} not named: {report}");
    }
    assert!(!on_disk.exists(), "one peer's entry was written");

    // And: the sweep reports it too, and still writes neither.
    let (code, report) = sweep_report(beta.path());
    assert_eq!(code, Some(3), "{report}");
    let problems = problems_of(&report);
    for named in [path.as_str(), "refs/knives/alpha", "refs/knives/gamma"] {
        assert!(problems.contains(named), "{named} not named: {report}");
    }
    assert!(!on_disk.exists(), "the sweep wrote one peer's entry");
}

#[test]
fn a_peer_entry_that_differs_from_the_file_here_leaves_the_file_and_says_so() {
    // Given: this machine has a file at an entry path, and a peer's ref
    // carries different contents at that path.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let beta = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(beta.path(), "beta", &remote);
    let path = format!("{FORK}/20261011T120000.000000000Z-0000.md");
    let on_disk = beta.path().join("ledger").join(&path);
    std::fs::create_dir_all(on_disk.parent().expect("the fork's directory")).expect("mkdir");
    let ours = peer_entry("this machine's account");
    std::fs::write(&on_disk, &ours).expect("write this machine's file");
    lab::push_as_peer(&remote, "alpha", &path, &peer_entry("alpha's account"));

    // When: status pulls.
    let (code, report) = status_on(&lab, beta.path());

    // Then: the mismatch is a problem naming the path and the ref, and the
    // file here is left exactly as it was.
    assert_eq!(code, Some(3), "{report}");
    let problems = problems_of(&report);
    for named in [path.as_str(), "refs/knives/alpha"] {
        assert!(problems.contains(named), "{named} not named: {report}");
    }
    assert_eq!(
        std::fs::read_to_string(&on_disk).expect("read this machine's file"),
        ours
    );
}

#[test]
fn two_peers_carrying_the_same_entry_at_one_path_write_it_once_and_say_nothing() {
    // Given: two peers' refs carry the identical entry under one file name.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let beta = shared_home(&lab);
    let (_remote_dir, remote) = lab::bare_remote();
    share(beta.path(), "beta", &remote);
    let path = format!("{FORK}/20261011T120000.000000000Z-0000.md");
    let entry = peer_entry("one account");
    lab::push_as_peer(&remote, "alpha", &path, &entry);
    lab::push_as_peer(&remote, "gamma", &path, &entry);
    let on_disk = beta.path().join("ledger").join(&path);

    for round in [
        "the first pull, which writes it",
        "the next pull, which finds it here",
    ] {
        // When: status pulls.
        let (code, report) = status_on(&lab, beta.path());

        // Then: the one entry is here, and nothing is a problem.
        assert_eq!(code, Some(0), "{round}: {report}");
        assert!(!problems_of(&report).contains(&path), "{round}: {report}");
        assert_eq!(
            std::fs::read_to_string(&on_disk).expect("the entry was written"),
            entry,
            "{round}"
        );
    }
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
        .read()
        .expect("read the ledger")
        .entries
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

/// Run `write` on the machine whose ledger root is `root` while its git
/// directory names no machine, as one not yet set up to share names none:
/// every pull and send there is refused before it starts, so what `write`
/// writes stays uncommitted, as a write whose send never ran leaves it.
fn with_no_machine_name<T>(root: &Path, write: impl FnOnce() -> T) -> T {
    let machine = lab::git_output(root, ["config", "--get", "knives.machine"]);
    lab::git_output(root, ["config", "--unset", "knives.machine"]);
    let output = write();
    lab::git_output(root, ["config", "knives.machine", &machine]);
    output
}

#[test]
fn a_second_machine_syncing_the_same_merge_writes_no_second_event() {
    // Given: two machines sharing one ledger remote, both of which saw #7 open.
    let lab = lab::Lab::new();
    let (_remote_dir, remote) = lab::bare_remote();
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &remote, &remote);

    // When: the forge reports #7 merged, alpha syncs, which sends its event,
    // and then beta syncs.
    let merged = forge_reporting("MERGED");
    let synced = sync_on(&lab, alpha.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    assert_eq!(recorded_on(&remote, MERGED), 1);
    let synced = sync_on(&lab, beta.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );

    // Then: beta's sync pulled alpha's event and wrote none of its own, so
    // its send had nothing to discard.
    assert_eq!(recorded_in(beta.path(), MERGED), 1);
    let (code, report) = status_on(&lab, beta.path());
    assert_eq!(code, Some(0), "{report}");
    assert!(
        !notes(&report).iter().any(|note| note.contains("discarded")),
        "{report}"
    );

    // And: once beta sweeps, the remote has the merge once.
    let swept = sweep(beta.path());
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );
    assert_eq!(recorded_in(beta.path(), MERGED), 1);
    assert_eq!(recorded_on(&remote, MERGED), 1);
    assert_eq!(entries_on(&remote, "beta"), 0);
}

#[test]
fn a_sweep_discards_its_own_uncommitted_repeat_of_a_transition_it_pulled() {
    // Given: two machines that saw #7 open.
    let lab = lab::Lab::new();
    let (_remote_dir, remote) = lab::bare_remote();
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &remote, &remote);

    // When: beta syncs the merge before alpha's event exists, while its
    // send cannot start, so its event stays uncommitted; then alpha syncs
    // it, which sends it, and then beta sweeps.
    let merged = forge_reporting("MERGED");
    let synced = with_no_machine_name(&beta.path().join("ledger"), || {
        sync_on(&lab, beta.path(), merged.path())
    });
    assert_eq!(
        synced.status.code(),
        Some(3),
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
    assert_eq!(recorded_on(&remote, MERGED), 1);
    let swept = sweep(beta.path());
    assert!(
        swept.status.success(),
        "{}",
        String::from_utf8_lossy(&swept.stdout)
    );

    // Then: beta's repeat is gone before any commit, it holds alpha's event
    // alone, and the remote has the merge once.
    assert_eq!(recorded_in(beta.path(), MERGED), 1);
    assert_eq!(recorded_on(&remote, MERGED), 1);
    assert_eq!(entries_on(&remote, "beta"), 0);
}

#[test]
fn status_names_the_repeated_transitions_a_writes_send_discarded() {
    // Given: two machines that saw #7 open, and beta's sync of the merge
    // while its send cannot start, so its event stays uncommitted.
    let lab = lab::Lab::new();
    let (_remote_dir, remote) = lab::bare_remote();
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &remote, &remote);
    let merged = forge_reporting("MERGED");
    let synced = with_no_machine_name(&beta.path().join("ledger"), || {
        sync_on(&lab, beta.path(), merged.path())
    });
    assert_eq!(
        synced.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    assert_eq!(recorded_in(beta.path(), MERGED), 1, "beta wrote its own");

    // When: alpha's event reaches the remote first, and then beta writes a
    // note, whose send discards beta's repeat and says nothing of it.
    let synced = sync_on(&lab, alpha.path(), merged.path());
    assert_eq!(
        synced.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&synced.stdout)
    );
    assert_eq!(recorded_on(&remote, MERGED), 1);
    let wrote = knives_on(
        &lab,
        beta.path(),
        &["--text", "notch", "-m", "after the merge", "--repo", "demo"],
    );
    assert!(
        wrote.status.success() && wrote.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&wrote.stderr)
    );
    assert_eq!(entries_on(&remote, "beta"), 1);
    assert_eq!(recorded_on(&remote, MERGED), 1);

    // Then: beta's status says its send discarded the repeat.
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
    let (remote_dir, remote) = lab::bare_remote();
    let alpha_view = remote_dir.path().join("alpha-view.git");
    let beta_view = remote_dir.path().join("beta-view.git");
    let (alpha, beta) = two_machines_that_saw_seven_open(&lab, &alpha_view, &beta_view);

    // When: each syncs the merge, and its send commits the event to its own
    // ref but cannot push it.
    let merged = forge_reporting("MERGED");
    for (home, machine) in [(&alpha, "alpha"), (&beta, "beta")] {
        let synced = sync_on(&lab, home.path(), merged.path());
        assert_eq!(
            synced.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&synced.stderr)
        );
        let git_dir = home.path().join("ledger").join(".git");
        assert!(
            git_in(
                &git_dir,
                &["rev-parse", "--verify", &format!("refs/knives/{machine}")],
            )
            .is_some(),
            "{machine}'s event was not committed"
        );
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
    }

    // Then: both committed events stay, on each machine and on the remote.
    assert_eq!(recorded_in(alpha.path(), MERGED), 2);
    assert_eq!(recorded_in(beta.path(), MERGED), 2);
    assert_eq!(recorded_on(&remote, MERGED), 2);
}
