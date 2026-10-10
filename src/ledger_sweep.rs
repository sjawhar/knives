//! Carrying the ledger between machines: the sweep a write hands off to, and
//! the pull a command runs before it decides something from the ledger.
//!
//! A write appends its entry and exits; [`hand_off`] starts `knives ledger
//! sweep` detached, so no write waits on the network or on another writer.
//! The sweep is single-flight. It takes the ledger's lock without waiting,
//! and a sweep that finds the lock taken exits at once, successfully: the
//! holder looks again before it lets go, so it carries whatever the second
//! one would have, and a burst of writes costs one sweep. Waiting instead
//! would put every writer of a burst back in line for one repository.
//!
//! A command that decides something from the ledger pulls first ([`pull`]),
//! and decides only from a ledger it could pull, or says it could not. A
//! report says so in its problems and still answers from the entries this
//! machine has, because the person reading it sees the staleness there. A
//! release write has nowhere to say so that would undo it, so the same
//! problem refuses it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::ConfigError;
use crate::ids::UpstreamName;
use crate::ledger::Ledger;
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
    #[error(transparent)]
    Ledger(#[from] crate::ledger::LedgerError),
    #[error(transparent)]
    Registry(#[from] ConfigError),
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

    fn carries(&self, fork: &UpstreamName) -> bool {
        self.repository.forks().contains(fork)
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.remote, self.repository.git_dir().display())
    }
}

/// Where the ledger at `root` travels, as the registry beside it says.
#[derive(Debug, Default)]
pub struct Destinations {
    /// The repositories it travels through.
    pub found: Vec<Destination>,
    /// Each fork whose registry entry names a repository its ledger belongs
    /// to that no destination here is, with that `<owner>/<name>`: its
    /// entries travel to and from no other machine from this one.
    pub unreached: BTreeMap<UpstreamName, String>,
}

impl Destinations {
    /// What a fork in [`Destinations::unreached`] is a problem for.
    fn unreached_problem(root: &Path, fork: &UpstreamName, ledger: &str) -> String {
        format!(
            "{fork}: repos.toml says its ledger belongs to {ledger}, but {} is not a \
             repository whose origin is {ledger}, so its entries travel to and from no other \
             machine here",
            root.join(".git").display()
        )
    }
}

/// Every destination the ledger at `root` travels through.
///
/// The one candidate is the repository at `<root>/.git`. It carries each
/// fork whose `repos.toml` entry, read from beside `root`, names its
/// `origin` as the repository the fork's ledger belongs to
/// ([`crate::config::RepoEntry::ledger`]), and it commits as the machine its
/// git config names, `knives.machine`, unique among the machines sharing
/// that remote. The registry is the one source of which forks travel where;
/// the repository's git config says only who this machine is.
///
/// A fork with no `ledger` is not shared. One whose `ledger` names a
/// repository `<root>/.git` is not, because the root is no repository or
/// its `origin` is another, is [`Destinations::unreached`]. No fork shared
/// and no machine named is a ledger nobody set up to share: no destination,
/// and nothing to report. A machine named with no fork shared is a
/// destination that carries nothing. A `knives.*` key this does not read,
/// or forks carried with no machine name, is refused, with the command that
/// fixes it.
pub fn destinations(root: &Path) -> Result<Destinations, SweepError> {
    let registry = crate::config::load(&root.with_file_name("repos.toml"))?;
    let git_dir = root.join(".git");
    let present = git_dir.try_exists().map_err(|source| SweepError::Read {
        path: git_dir.clone(),
        source,
    })?;
    let refused = |detail: String| SweepError::Config {
        git_dir: git_dir.clone(),
        detail,
    };
    let mut machine = None;
    let mut origin = None;
    if present {
        for (key, value) in
            ledger_git::local_config(&git_dir, "^(knives\\..*|remote\\.origin\\.url)$")?
        {
            match key.as_str() {
                "knives.machine" => machine = Some(value),
                "remote.origin.url" => origin = Some(value),
                _ => {
                    return Err(refused(format!(
                        "sets {key}, which knives does not read: it reads knives.machine, and \
                         which forks a repository carries is each repos.toml entry's `ledger`. \
                         Remove it: git --git-dir={} config --unset-all {key}",
                        git_dir.display()
                    )));
                }
            }
        }
    }
    let slug = origin.as_deref().and_then(crate::remote_url::remote_slug);
    let mut carried = Vec::new();
    let mut unreached = BTreeMap::new();
    for (key, entry) in &registry.repos {
        let Some(ledger) = &entry.ledger else {
            continue;
        };
        if slug.is_some_and(|slug| entry.shares_ledger_with(slug)) {
            carried.push(entry.upstream_name(key));
        } else {
            let _ = unreached.insert(entry.upstream_name(key), ledger.clone());
        }
    }
    let found = match machine {
        Some(machine) => vec![Destination {
            repository: Repository::new(&git_dir, root, carried)?,
            remote: REMOTE.to_owned(),
            machine,
        }],
        None if carried.is_empty() => Vec::new(),
        None => {
            let names: Vec<String> = carried.iter().map(ToString::to_string).collect();
            return Err(refused(format!(
                "carries the ledgers of {}, but names no machine, so nothing can be committed \
                 as this one. Name this machine, uniquely among every machine sharing \
                 {REMOTE}: git --git-dir={} config knives.machine <name>",
                names.join(", "),
                git_dir.display()
            )));
        }
    };
    Ok(Destinations { found, unreached })
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
    /// Transition events this machine wrote that repeated one another
    /// machine had already committed, removed before the commit.
    pub discarded: usize,
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
            discarded: 0,
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
    /// No destination, and no fork the registry shares: this machine's
    /// ledger is not shared.
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
///
/// Each [`Destinations::unreached`] fork is one of the sweep's problems, so
/// it is in the [`sweep_log`] until the ledger root is the repository its
/// registry entry names.
pub fn sweep(root: &Path, destinations: &Destinations) -> Result<Swept, SweepError> {
    let Destinations { found, unreached } = destinations;
    if found.is_empty() && unreached.is_empty() {
        return Ok(Swept::NotShared);
    }
    let mut problems: Vec<String> = unreached
        .iter()
        .map(|(fork, ledger)| Destinations::unreached_problem(root, fork, ledger))
        .collect();
    let mut tallies: Vec<Tally> = found.iter().map(Tally::of).collect();
    let mut held = false;
    while let Some(lock) = FileLock::try_acquire(root)? {
        held = true;
        loop {
            for (destination, tally) in found.iter().zip(&mut tallies) {
                if tally.active() {
                    let failures = pass(destination, tally);
                    tally.failed = !failures.is_empty();
                    problems.extend(failures);
                }
            }
            if !anything_new(found, &mut tallies, &mut problems) {
                break;
            }
        }
        record(root, &problems)?;
        drop(lock);
        if !anything_new(found, &mut tallies, &mut problems) {
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
        failures.push(failed(error));
    }
    failures
}

/// Commit what is new and push what the remote lacks.
///
/// Before the commit, each transition event this machine wrote that repeats
/// one a ref already carries goes
/// ([`crate::commands::sync::repeated_transitions`]): two machines that
/// both synced after one merge each wrote `#N merged`, and the one that
/// sweeps second commits none. Only an entry no ref carries is removed, so
/// what any machine committed stays.
fn send(destination: &Destination, tally: &mut Tally) -> Result<(), SweepError> {
    let repository = &destination.repository;
    let mut pending = ledger_git::pending(repository, &destination.machine)?;
    if !pending.added().is_empty() {
        let uncommitted: BTreeSet<PathBuf> = pending
            .added()
            .iter()
            .map(|path| repository.work_tree().join(path))
            .collect();
        for fork in repository.forks() {
            let entries = Ledger::at(repository.work_tree().join(fork.as_str())).entry_files()?;
            for path in crate::commands::sync::repeated_transitions(&entries, &uncommitted) {
                std::fs::remove_file(path).map_err(|source| SweepError::Write {
                    path: path.to_owned(),
                    source,
                })?;
                if let Ok(relative) = path.strip_prefix(repository.work_tree()) {
                    pending.forget(relative);
                }
                tally.discarded += 1;
            }
        }
    }
    if ledger_git::commit_pending(repository, &destination.machine, pending)?.is_some() {
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

/// What pulling the ledger before a decision found, per fork asked about.
#[derive(Debug, Default)]
pub struct Pulled {
    root: PathBuf,
    destinations: Vec<Destination>,
    problems: BTreeMap<UpstreamName, Vec<String>>,
}

impl Pulled {
    /// The problems a report about `fork` carries: a pull that failed, or
    /// destinations that could not be read.
    pub fn problems_for(&self, fork: &UpstreamName) -> Vec<String> {
        self.problems.get(fork).cloned().unwrap_or_default()
    }

    /// How many of `fork`'s entries its remote does not have yet, and where
    /// the last sweep's failures are when it failed; `None` when neither.
    pub fn backlog_for(&self, fork: &UpstreamName) -> Option<String> {
        let destination = self
            .destinations
            .iter()
            .find(|destination| destination.carries(fork))?;
        let unsent =
            ledger_git::unsent(&destination.repository, &destination.remote).map(|paths| {
                paths
                    .iter()
                    .filter(|path| path.starts_with(fork.as_str()))
                    .count()
            });
        let log = sweep_log(&self.root);
        let failed = log
            .exists()
            .then(|| format!("the last ledger sweep failed: see {}", log.display()));
        let waiting = match unsent {
            Ok(0) => None,
            Ok(1) => Some(format!(
                "1 ledger entry not yet on {}",
                destination.describe()
            )),
            Ok(count) => Some(format!(
                "{count} ledger entries not yet on {}",
                destination.describe()
            )),
            Err(error) => Some(format!(
                "could not count ledger entries not yet on {}: {error}",
                destination.describe()
            )),
        };
        match (waiting, failed) {
            (Some(waiting), Some(failed)) => Some(format!("{waiting}; {failed}")),
            (waiting, failed) => waiting.or(failed),
        }
    }
}

/// Pull the ledgers of `forks` from every destination that carries one,
/// before a command decides something from them.
pub fn pull(forks: &[&UpstreamName]) -> Pulled {
    pull_at(&crate::ledger::default_ledger_root(), forks)
}

/// [`pull`] for the ledger at `root`.
///
/// Each destination's fetch waits for its transport lock, so a pull racing a
/// sweep reads what the sweep left rather than failing on it. A failure is a
/// problem for every fork asked about that the destination carries, never an
/// error: the command still answers from the entries this machine has. So
/// is a fork asked about whose registry entry names a repository its ledger
/// belongs to that no destination here is ([`Destinations::unreached`]).
pub fn pull_at(root: &Path, forks: &[&UpstreamName]) -> Pulled {
    let mut pulled = Pulled {
        root: root.to_owned(),
        ..Pulled::default()
    };
    if forks.is_empty() {
        return pulled;
    }
    let Destinations { found, unreached } = match destinations(root) {
        Ok(found) => found,
        Err(error) => {
            for fork in forks {
                pulled
                    .problems
                    .entry((*fork).clone())
                    .or_default()
                    .push(format!("could not pull the ledger: {error}"));
            }
            return pulled;
        }
    };
    let asked: BTreeSet<&UpstreamName> = forks.iter().copied().collect();
    for (fork, ledger) in &unreached {
        if asked.contains(fork) {
            pulled
                .problems
                .entry(fork.clone())
                .or_default()
                .push(Destinations::unreached_problem(root, fork, ledger));
        }
    }
    for destination in found {
        let carried: Vec<&UpstreamName> = destination
            .repository
            .forks()
            .iter()
            .filter(|fork| asked.contains(fork))
            .collect();
        if carried.is_empty() {
            continue;
        }
        if let Err(error) = pull_one(&destination) {
            let problem = format!(
                "could not pull the ledger from {}: {error}; this answers from the entries \
                 this machine has",
                destination.describe()
            );
            for fork in carried {
                pulled
                    .problems
                    .entry(fork.clone())
                    .or_default()
                    .push(problem.clone());
            }
        }
        pulled.destinations.push(destination);
    }
    pulled
}

fn pull_one(destination: &Destination) -> Result<usize, SweepError> {
    let _transport = FileLock::acquire(&destination.transport_lock(), LockWait::TRANSPORT)?;
    let refs = ledger_git::fetch(&destination.repository, &destination.remote)?;
    Ok(ledger_git::materialise(&destination.repository, &refs)?)
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

    /// The repository forks share their ledgers through in these tests.
    const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";

    /// Write a registry beside `root` naming `forks`, each `(key, ledger)`:
    /// the fork `acme/<key>`, sharing its ledger with `ledger` when given.
    fn registry(root: &Path, forks: &[(&str, Option<&str>)]) {
        use std::fmt::Write as _;
        let mut text = String::new();
        for (key, ledger) in forks {
            let _ = writeln!(
                text,
                "[repos.{key}]\nupstream = \"https://forge.invalid/acme/{key}\"\n\
                 origin = \"https://forge.invalid/ours/{key}\"\n{}",
                ledger.map_or_else(String::new, |ledger| format!("ledger = \"{ledger}\"\n"))
            );
        }
        std::fs::write(root.with_file_name("repos.toml"), text).unwrap();
    }

    /// Give `root`'s repository [`LEDGER_URL`] as its `origin`, which git
    /// reaches at `reached`.
    fn origin_at(root: &Path, reached: &str) {
        let git_dir = root.join(".git");
        git(&git_dir, &["remote", "add", "origin", LEDGER_URL]);
        git(
            &git_dir,
            &["config", &format!("url.{reached}.insteadOf"), LEDGER_URL],
        );
    }

    #[test]
    fn a_ledger_no_fork_shares_and_no_machine_names_is_not_shared() {
        let home = tempfile::tempdir().unwrap();
        let missing = destinations(&home.path().join("ledger")).unwrap();
        assert!(missing.found.is_empty() && missing.unreached.is_empty());

        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", None)]);
        origin_at(&root, "/nonexistent/remote.git");
        let unshared = destinations(&root).unwrap();
        assert!(unshared.found.is_empty() && unshared.unreached.is_empty());
    }

    #[test]
    fn forks_with_no_machine_name_are_refused_with_the_command_that_names_one() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", Some("acme/ledger"))]);
        origin_at(&root, "/nonexistent/remote.git");

        let error = destinations(&root).unwrap_err().to_string();

        assert!(error.contains("acme/a-repo"), "was: {error}");
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
    fn the_root_carries_each_fork_whose_registry_entry_names_its_origin() {
        // Given: two forks sharing their ledger with the root's origin, one
        // spelling it in other letter case with `.git`, and one not shared.
        let (_home, root) = ledger_root();
        registry(
            &root,
            &[
                ("a-repo", Some("acme/ledger")),
                ("b-repo", Some("Acme/Ledger.git")),
                ("c-repo", None),
            ],
        );
        origin_at(&root, "/nonexistent/remote.git");
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);

        let found = destinations(&root).unwrap();

        assert!(found.unreached.is_empty(), "{:?}", found.unreached);
        assert_eq!(found.found.len(), 1);
        let destination = found.found.first().unwrap();
        assert_eq!(destination.machine, "alpha");
        assert_eq!(destination.remote, "origin");
        assert_eq!(
            destination.repository.forks(),
            &BTreeSet::from([
                UpstreamName::new("acme/a-repo"),
                UpstreamName::new("acme/b-repo")
            ])
        );
    }

    #[test]
    fn a_fork_whose_ledger_is_no_repository_here_is_unreached_and_a_pull_says_so() {
        // Given: a fork sharing its ledger with a repository the root's
        // origin is not, and a second machine whose root is no repository.
        let (_home, root) = ledger_root();
        registry(
            &root,
            &[("a-repo", Some("acme/elsewhere")), ("b-repo", None)],
        );
        origin_at(&root, "/nonexistent/remote.git");
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
        let bare = tempfile::tempdir().unwrap();
        let unrepo = bare.path().join("ledger");
        registry(&unrepo, &[("a-repo", Some("acme/ledger"))]);

        // Then: the root carries neither fork, and names the one it should.
        let a_repo = UpstreamName::new("acme/a-repo");
        for (root, ledger) in [(&root, "acme/elsewhere"), (&unrepo, "acme/ledger")] {
            let found = destinations(root).unwrap();
            assert!(
                found
                    .found
                    .iter()
                    .all(|destination| destination.repository.forks().is_empty()),
                "{found:?}"
            );
            assert_eq!(
                found.unreached,
                BTreeMap::from([(a_repo.clone(), ledger.to_owned())])
            );

            // And: a pull about it reports that its entries reach no machine.
            let problems = pull_at(root, &[&a_repo]).problems_for(&a_repo);
            assert_eq!(problems.len(), 1, "{problems:?}");
            let problem = problems.first().unwrap();
            assert!(
                problem.contains(&format!("its ledger belongs to {ledger}")),
                "{problem}"
            );
        }
    }

    #[test]
    fn a_machine_name_with_no_fork_carries_nothing_and_a_pull_asks_no_remote() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", None)]);
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
        // A remote that cannot answer: a pull that tried it would fail.
        origin_at(&root, "/nonexistent/remote.git");

        let a_repo = UpstreamName::new("acme/a-repo");
        let pulled = pull_at(&root, &[&a_repo]);

        assert!(pulled.problems_for(&a_repo).is_empty());
        assert_eq!(pulled.backlog_for(&a_repo), None);
    }

    #[test]
    fn a_pull_that_fails_is_a_problem_for_the_forks_it_carries_only() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", Some("acme/ledger")), ("b-repo", None)]);
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
        origin_at(&root, "/nonexistent/remote.git");

        let (a_repo, b_repo) = (
            UpstreamName::new("acme/a-repo"),
            UpstreamName::new("acme/b-repo"),
        );
        let pulled = pull_at(&root, &[&a_repo, &b_repo]);

        let problems = pulled.problems_for(&a_repo);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems
                .iter()
                .all(|problem| problem.contains("could not pull the ledger from origin")),
            "{problems:?}"
        );
        assert!(pulled.problems_for(&b_repo).is_empty());
    }
}
