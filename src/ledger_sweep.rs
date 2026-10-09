//! Carrying the ledger between machines: the sweep a write hands off to.
//!
//! A write appends its entry and exits; [`hand_off`] starts `knives ledger
//! sweep` detached, so no write waits on the network or on another writer.
//! The sweep is single-flight. It takes the ledger's lock without waiting,
//! and a sweep that finds the lock taken exits at once, successfully: the
//! holder looks again before it lets go, so it carries whatever the second
//! one would have, and a burst of writes costs one sweep. Waiting instead
//! would put every writer of a burst back in line for one repository.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::ids::RepoName;
use crate::ledger_git::{self, GitError, Repository};
use crate::lock::{FileLock, LockError, LockWait};

/// The remote every destination pushes to and pulls from.
const REMOTE: &str = "origin";

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("{git_dir}: {detail}")]
    Config { git_dir: PathBuf, detail: String },
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// One repository the ledger travels through, and this machine's name there.
#[derive(Debug, Clone)]
pub struct Destination {
    pub repository: Repository,
    pub remote: String,
    pub machine: String,
}

impl Destination {
    /// What every fetch, materialise and commit through this repository holds.
    ///
    /// Two fetches into one repository fail on each other's ref locks, and a
    /// commit that lists the root while a pull writes a peer's entries into
    /// it takes one of them for this machine's own. In the git directory, so
    /// it is outside every fork's directory and no commit can carry it.
    fn transport_lock(&self) -> PathBuf {
        self.repository.git_dir().join("knives-transport")
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.remote, self.repository.git_dir().display())
    }
}

/// Every destination the ledger at `root` travels through.
///
/// Provisional. Today the one candidate is the repository at `<root>/.git`,
/// and what it carries is read from that repository's own git config:
/// `knives.machine`, this machine's name among those sharing its remote, and
/// `knives.fork`, once per fork it carries. `repos.toml` replaces this
/// function: each fork's registry entry names the repository its ledger
/// belongs to, and these git-config keys are then deleted, not kept beside it
/// as a fallback, so that which forks a repository carries has one source.
/// Under either, a repository that names no fork carries nothing.
///
/// No `<root>/.git`, or one that sets no `knives.*` key, is a ledger nobody
/// set up to share: no destination, and nothing to report. A `knives.*` key
/// this does not read, or forks with no machine name, is refused, with the
/// command that fixes it.
pub fn destinations(root: &Path) -> Result<Vec<Destination>, SweepError> {
    let git_dir = root.join(".git");
    let present = git_dir.try_exists().map_err(|source| SweepError::Read {
        path: git_dir.clone(),
        source,
    })?;
    if !present {
        return Ok(Vec::new());
    }
    let config = ledger_git::knives_config(&git_dir)?;
    if config.is_empty() {
        return Ok(Vec::new());
    }
    let refused = |detail: String| SweepError::Config {
        git_dir: git_dir.clone(),
        detail,
    };
    let mut machine = None;
    let mut forks = Vec::new();
    for (key, value) in config {
        match key.as_str() {
            "knives.machine" => machine = Some(value),
            "knives.fork" => forks.push(RepoName::new(value)),
            _ => {
                return Err(refused(format!(
                    "sets {key}, which knives does not read; it reads knives.machine and \
                     knives.fork. Remove it: git --git-dir={} config --unset-all {key}",
                    git_dir.display()
                )));
            }
        }
    }
    let Some(machine) = machine else {
        return Err(refused(format!(
            "carries ledger forks but names no machine, so nothing can be committed as this \
             one. Name this machine, uniquely among every machine sharing {REMOTE}: \
             git --git-dir={} config knives.machine <name>",
            git_dir.display()
        )));
    };
    Ok(vec![Destination {
        repository: Repository::new(&git_dir, root, forks)?,
        remote: REMOTE.to_owned(),
        machine,
    }])
}

/// Where the last sweep's failures are, and only while the last sweep failed.
pub fn sweep_log(root: &Path) -> PathBuf {
    root.with_file_name("ledger-sweep.log")
}

/// What one destination's passes did over one sweep.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Tally {
    pub git_dir: String,
    pub remote: String,
    pub forks: Vec<String>,
    pub commits: usize,
    pub pulled: usize,
    pub pushes: usize,
    #[serde(skip)]
    failed: bool,
}

impl Tally {
    fn of(destination: &Destination) -> Self {
        Self {
            git_dir: destination.repository.git_dir().display().to_string(),
            remote: destination.remote.clone(),
            forks: destination
                .repository
                .forks()
                .iter()
                .map(ToString::to_string)
                .collect(),
            commits: 0,
            pulled: 0,
            pushes: 0,
            failed: false,
        }
    }

    /// Whether another pass has anything to do: a destination that failed
    /// is not retried within one sweep, or one that keeps failing with
    /// entries waiting would hold the lock forever.
    const fn active(&self) -> bool {
        !self.failed && !self.forks.is_empty()
    }
}

/// What a sweep did.
#[derive(Debug)]
pub enum Swept {
    /// No destination: this machine's ledger is not shared.
    NotShared,
    /// Another sweep holds the lock, and carries what this one would have.
    Busy,
    /// This sweep held the lock: each destination's tally, and every failure.
    Ran {
        tallies: Vec<Tally>,
        problems: Vec<String>,
    },
}

/// Sweep the ledger at `root` through every destination it has.
///
/// A destination that cannot be read is a failed sweep: it is written to the
/// [`sweep_log`] when no other sweep holds the lock, and returned.
pub fn run(root: &Path) -> Result<Swept, SweepError> {
    match destinations(root) {
        Ok(found) if found.is_empty() => Ok(Swept::NotShared),
        Ok(found) => sweep(root, &found),
        Err(error) => {
            if let Some(_lock) = FileLock::try_acquire(root)? {
                record(root, &[error.to_string()])?;
            }
            Err(error)
        }
    }
}

/// Commit, pull and push each of `destinations`, single-flight.
///
/// The lock beside `root` is taken without waiting; finding it taken is
/// [`Swept::Busy`]. The holder passes over every destination, looks again,
/// and lets go only when a look finds nothing new. After letting go it looks
/// once more: an entry written while it held the lock was handed to a sweep
/// that found the lock taken and left, so either the holder takes the lock
/// back for it, or the sweep that took the lock in between carries it.
pub fn sweep(root: &Path, destinations: &[Destination]) -> Result<Swept, SweepError> {
    if destinations.is_empty() {
        return Ok(Swept::NotShared);
    }
    let mut tallies: Vec<Tally> = destinations.iter().map(Tally::of).collect();
    let mut problems = Vec::new();
    let mut held = false;
    while let Some(lock) = FileLock::try_acquire(root)? {
        held = true;
        loop {
            for (destination, tally) in destinations.iter().zip(&mut tallies) {
                if tally.active() {
                    let failures = pass(destination, tally);
                    tally.failed = !failures.is_empty();
                    problems.extend(failures);
                }
            }
            if !anything_new(destinations, &mut tallies, &mut problems) {
                break;
            }
        }
        record(root, &problems)?;
        drop(lock);
        if !anything_new(destinations, &mut tallies, &mut problems) {
            break;
        }
    }
    Ok(if held {
        Swept::Ran { tallies, problems }
    } else {
        Swept::Busy
    })
}

/// One destination's pull, then commit and push, under its transport lock;
/// each failure as a line naming the destination.
///
/// The pull goes first, so a remote holding this machine's ref for a
/// checkout that never committed as it is seen, and the commit refused,
/// rather than a second history started and its push rejected. A failed
/// pull does not stop the commit and push: this machine's entries leave
/// whether or not a peer's ref can be read.
fn pass(destination: &Destination, tally: &mut Tally) -> Vec<String> {
    let failed = |error: SweepError| format!("{}: {error}", destination.describe());
    let _transport = match FileLock::acquire(&destination.transport_lock(), LockWait::TRANSPORT) {
        Ok(lock) => lock,
        Err(error) => return vec![failed(error.into())],
    };
    let repository = &destination.repository;
    let mut failures = Vec::new();
    match ledger_git::fetch(repository, &destination.remote)
        .and_then(|refs| ledger_git::materialise(repository, &refs))
    {
        Ok(written) => tally.pulled += written,
        Err(error) => failures.push(failed(error.into())),
    }
    if let Err(error) = send(destination, tally) {
        failures.push(failed(error.into()));
    }
    failures
}

/// Commit what is new and push what the remote lacks.
fn send(destination: &Destination, tally: &mut Tally) -> Result<(), GitError> {
    let repository = &destination.repository;
    if ledger_git::commit_new_entries(repository, &destination.machine)?.is_some() {
        tally.commits += 1;
    }
    if ledger_git::unpushed(repository, &destination.remote, &destination.machine)? {
        ledger_git::push(repository, &destination.remote, &destination.machine)?;
        tally.pushes += 1;
    }
    Ok(())
}

/// Whether any destination still being swept has an entry no ref carries.
fn anything_new(
    destinations: &[Destination],
    tallies: &mut [Tally],
    problems: &mut Vec<String>,
) -> bool {
    let mut any = false;
    for (destination, tally) in destinations.iter().zip(tallies) {
        if !tally.active() {
            continue;
        }
        match ledger_git::has_new_entries(&destination.repository, &destination.machine) {
            Ok(new) => any |= new,
            Err(error) => {
                tally.failed = true;
                problems.push(format!("{}: {error}", destination.describe()));
            }
        }
    }
    any
}

/// Leave `problems` in the sweep log, or remove it when there are none.
/// Called with the lock held, so a later sweep's record is never overwritten
/// by an earlier one's.
fn record(root: &Path, problems: &[String]) -> Result<(), SweepError> {
    let log = sweep_log(root);
    if problems.is_empty() {
        return match std::fs::remove_file(&log) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SweepError::Write { path: log, source }),
        };
    }
    let mut text = format!("{} the last ledger sweep failed:\n", jiff::Timestamp::now());
    for problem in problems {
        text.push_str(problem);
        text.push('\n');
    }
    std::fs::write(&log, text).map_err(|source| SweepError::Write { path: log, source })
}

/// Start `knives ledger sweep` detached, and return without waiting for it.
///
/// Called once, as the process exits, by a command that appended an entry
/// ([`crate::ledger::appended`]). The entry is already on disk, so a sweep
/// that never starts loses nothing: the next one carries it. The sweep's
/// output goes nowhere, because the caller has stopped listening by the time
/// it finishes; a sweep that fails says so in [`sweep_log`]. It runs in a
/// process group of its own, so the Ctrl-C that stops the caller does not
/// stop it mid-push.
pub fn hand_off() {
    use std::os::unix::process::CommandExt as _;
    let started = std::time::Instant::now();
    let spawned = std::env::current_exe().and_then(|executable| {
        std::process::Command::new(executable)
            .args(["ledger", "sweep"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
    });
    match spawned {
        // Never waited on: it is meant to outlive this process, and init
        // reaps it once this process exits.
        Ok(child) => drop(child),
        Err(error) => eprintln!(
            "knives: could not start the ledger sweep ({error}); the entry is on disk, and \
             the next sweep carries it"
        ),
    }
    if crate::timing::enabled() {
        eprintln!("timing ledger hand-off {}us", started.elapsed().as_micros());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn git(git_dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn ledger_root() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("ledger");
        std::fs::create_dir_all(&root).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .status()
            .unwrap();
        assert!(status.success());
        (home, root)
    }

    #[test]
    fn a_ledger_with_no_git_directory_or_no_knives_keys_is_not_shared() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            destinations(&home.path().join("ledger"))
                .unwrap()
                .is_empty()
        );

        let (_home, root) = ledger_root();
        assert!(destinations(&root).unwrap().is_empty());
    }

    #[test]
    fn forks_with_no_machine_name_are_refused_with_the_command_that_names_one() {
        let (_home, root) = ledger_root();
        git(&root.join(".git"), &["config", "knives.fork", "a-repo"]);

        let error = destinations(&root).unwrap_err().to_string();

        assert!(
            error.contains(&format!(
                "git --git-dir={} config knives.machine <name>",
                root.join(".git").display()
            )),
            "was: {error}"
        );
    }

    #[test]
    fn a_knives_key_nothing_reads_is_refused() {
        let (_home, root) = ledger_root();
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
        git(&root.join(".git"), &["config", "knives.forks", "a-repo"]);

        let error = destinations(&root).unwrap_err().to_string();

        assert!(error.contains("knives.forks"), "was: {error}");
        assert!(error.contains("--unset-all knives.forks"), "was: {error}");
    }

    #[test]
    fn a_machine_name_and_its_forks_make_one_destination() {
        let (_home, root) = ledger_root();
        let git_dir = root.join(".git");
        git(&git_dir, &["config", "knives.machine", "alpha"]);
        git(&git_dir, &["config", "--add", "knives.fork", "a-repo"]);
        git(&git_dir, &["config", "--add", "knives.fork", "b-repo"]);

        let found = destinations(&root).unwrap();

        assert_eq!(found.len(), 1);
        let destination = found.first().unwrap();
        assert_eq!(destination.machine, "alpha");
        assert_eq!(destination.remote, "origin");
        assert_eq!(
            destination.repository.forks(),
            &BTreeSet::from([RepoName::new("a-repo"), RepoName::new("b-repo")])
        );
    }
}
