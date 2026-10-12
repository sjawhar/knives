//! Carrying the ledger between machines: the sweep a write hands off to, and
//! the pull a command runs before it decides something from the ledger.
//!
//! A write appends its entry and exits; [`hand_off`] starts `knives ledger
//! sweep` detached, so no write waits on the network or on another writer.
//! The sweep is single-flight. The one a write hands off takes the ledger's
//! lock without waiting, and exits at once, successfully, when it finds the
//! lock taken: the holder looks again before it lets go, so it carries
//! whatever the second one would have, and a burst of writes costs one
//! sweep. Waiting instead would put every writer of a burst back in line for
//! one repository. A sweep run by hand waits for the holder instead, then
//! runs its own pass, because its caller reads its outcome: a holder's
//! failure goes only to the [`sweep_log`].
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

use crate::config::{ConfigError, Registry};
use crate::ids::UpstreamName;
use crate::ledger::Ledger;
use crate::ledger_git::{self, GitError};
use crate::lock::{FileLock, LockError, LockHolder, LockWait};

mod destinations;
mod kept;
mod pulled;

pub use destinations::{Destination, Destinations, destinations, ledger_repositories, unpullable};
use kept::keep;
pub use pulled::{Backlog, Pulled, pull, pull_at};

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
    #[error(
        "{path} is not a sweep record this knives can read ({source}); remove it, and the \
         next sweep writes it afresh"
    )]
    Record {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// The problem each of `forks` has from where the ledger at `root` travels:
/// every one of them, when that could not be read; else each fork no git
/// directory reaches, as its `unreached` entry says.
fn setup_problems(
    root: &Path,
    forks: &[&UpstreamName],
    unreached: Result<&BTreeMap<UpstreamName, String>, &SweepError>,
) -> BTreeMap<UpstreamName, Vec<String>> {
    forks
        .iter()
        .filter_map(|fork| {
            let problem = match unreached {
                Err(error) => format!("could not pull the ledger: {error}"),
                Ok(unreached) => Destinations::unreached_problem(root, fork, unreached.get(*fork)?),
            };
            Some(((*fork).clone(), vec![problem]))
        })
        .collect()
}

/// Where the last sweep's failures are, and only while the last sweep failed.
pub fn sweep_log(root: &Path) -> PathBuf {
    root.with_file_name("ledger-sweep.log")
}

/// Where sweeps keep what they did that nobody running them sees
/// ([`Kept`]): a sweep a write hands off prints to nowhere.
pub fn sweep_record(root: &Path) -> PathBuf {
    root.with_file_name("ledger-sweep.json")
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
    /// By the fork directory they are in, the entries other machines sent
    /// through this repository that this machine does not read, because it
    /// carries no such fork there; as the sweep's last pull found them, and
    /// absent when none succeeded or it found none.
    #[serde(skip_serializing_if = "skipped_nothing")]
    pub skipped: Option<BTreeMap<String, usize>>,
    /// How many of this machine's entries, in the forks this repository
    /// carries, no fetched ref holds when the sweep ends
    /// ([`ledger_git::unsent`]): what a failed sweep leaves on this machine
    /// alone. `0` for a repository carrying no fork. Absent when it could
    /// not be counted, which a problem says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsent: Option<usize>,
    /// By fork, the discards not yet in the [`sweep_record`].
    #[serde(skip)]
    unkept: BTreeMap<String, usize>,
    /// Each peer entry the sweep's last pull did not write
    /// ([`ledger_git::Materialised::unwritten`]): one it could not read, or
    /// one whose contents disagree. A problem of the sweep, never a failure
    /// of the pass.
    #[serde(skip)]
    unwritten: Vec<String>,
    #[serde(skip)]
    failed: bool,
}

#[expect(
    clippy::ref_option,
    reason = "serde's skip_serializing_if hands the field over by reference"
)]
fn skipped_nothing(skipped: &Option<BTreeMap<String, usize>>) -> bool {
    skipped.as_ref().is_none_or(BTreeMap::is_empty)
}

impl Tally {
    fn of(destination: &Destination) -> Self {
        Self {
            git_dir: destination.repository.git_dir().display().to_string(),
            remote: REMOTE.to_owned(),
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
            skipped: None,
            // A repository carrying no fork holds nothing of this machine's
            // to send; [`sweep`] counts every other one when it ends.
            unsent: destination.repository.forks().is_empty().then_some(0),
            unkept: BTreeMap::new(),
            unwritten: Vec::new(),
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
    /// Another sweep holds the lock, and carries what this one would have:
    /// the answer of a sweep that leaves when it finds the lock taken
    /// ([`Contended::Leave`]).
    Busy,
    /// This sweep held the lock: each destination's tally, every failure,
    /// what it passed over without failing ([`Destinations::ignored`]), and
    /// the sweep it waited for first, if it did.
    Ran {
        tallies: Vec<Tally>,
        problems: Vec<String>,
        notes: Vec<String>,
        waited: Option<Waited>,
    },
}

/// What a sweep does when another holds the ledger's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contended {
    /// Leave at once, as [`Swept::Busy`]: the sweep a write hands off, whose
    /// output goes nowhere and whose work the holder carries.
    Leave,
    /// Wait for the holder, as long as a ledger repository's transport lock
    /// is waited for, then run its own pass: a sweep run by hand, whose
    /// caller reads its outcome and exit code.
    Wait,
}

/// The sweep a sweep run by hand waited for before it ran.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Waited {
    /// The pid the lock file named when the wait began; absent when it
    /// named none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// How long this sweep waited, in whole seconds.
    pub seconds: u64,
}

/// Set on the sweep [`hand_off`] starts, so that it leaves at once when it
/// finds another sweep running ([`Contended::Leave`]). Not for anyone else
/// to set.
pub const HANDED_OFF: &str = "KNIVES_LEDGER_HANDED_OFF";

/// Sweep the ledger at `root` through every destination `registry` gives
/// it, doing what `contended` says when another sweep holds the lock.
///
/// A registry that could not be read, or a destination that cannot be,
/// is a failed sweep: it is written to the [`sweep_log`] when no other
/// sweep holds the lock, and returned.
pub fn run(
    root: &Path,
    registry: Result<Registry, ConfigError>,
    contended: Contended,
) -> Result<Swept, SweepError> {
    match registry
        .map_err(SweepError::from)
        .and_then(|registry| destinations(root, &registry))
    {
        Ok(found) => sweep(root, &found, contended),
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
/// The lock beside `root` is taken as `contended` says: without waiting,
/// when finding it taken is [`Swept::Busy`], or after waiting for its
/// holder, giving up as a failure if it is still held at the transport
/// lock's deadline. The holder passes over every destination, looks again,
/// and lets go only when a look finds nothing new. After letting go it looks
/// once more: an entry written while it held the lock was handed to a sweep
/// that found the lock taken and left, so either the holder takes the lock
/// back for it, or the sweep that took the lock in between carries it.
///
/// Each [`Destinations::unreached`] fork, each [`Destinations::unclaimed`]
/// directory and each peer entry the last pull through a destination did not
/// write ([`ledger_git::Materialised::unwritten`]) is one of the sweep's
/// problems, so it is in the [`sweep_log`] until it is fixed. What the
/// passes discarded and left unread goes to the [`sweep_record`] before the
/// lock is let go.
pub fn sweep(
    root: &Path,
    destinations: &Destinations,
    contended: Contended,
) -> Result<Swept, SweepError> {
    let Destinations {
        found,
        unreached,
        unclaimed,
        ignored,
    } = destinations;
    if found.is_empty() && unreached.is_empty() && unclaimed.is_empty() {
        return Ok(Swept::NotShared);
    }
    let mut problems: Vec<String> = unreached
        .iter()
        .map(|(fork, ledger)| Destinations::unreached_problem(root, fork, ledger))
        .chain(unclaimed.iter().cloned())
        .collect();
    let mut tallies: Vec<Tally> = found.iter().map(Tally::of).collect();
    let unwritten = |tallies: &[Tally]| -> Vec<String> {
        tallies
            .iter()
            .flat_map(|tally| tally.unwritten.iter().cloned())
            .collect()
    };
    let (next, waited) = match contended {
        Contended::Leave => (FileLock::try_acquire(root)?, None),
        Contended::Wait => {
            let started = std::time::Instant::now();
            let (lock, holder) = FileLock::acquire_noting_holder(root, LockWait::TRANSPORT)?;
            let waited = holder.map(|holder: LockHolder| Waited {
                pid: holder.pid,
                seconds: started.elapsed().as_secs(),
            });
            (Some(lock), waited)
        }
    };
    let Some(mut lock) = next else {
        return Ok(Swept::Busy);
    };
    loop {
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
        if let Err(error) = keep(root, found, &mut tallies) {
            problems.push(error.to_string());
        }
        let mut recorded = problems.clone();
        recorded.extend(unwritten(&tallies));
        record(root, &recorded)?;
        drop(lock);
        if !anything_new(found, &mut tallies, &mut problems) {
            break;
        }
        match FileLock::try_acquire(root)? {
            Some(again) => lock = again,
            None => break,
        }
    }
    problems.extend(unwritten(&tallies));
    for (destination, tally) in found.iter().zip(&mut tallies) {
        if destination.repository.forks().is_empty() {
            continue;
        }
        tally.unsent = match ledger_git::unsent(&destination.repository, REMOTE) {
            Ok(paths) => Some(paths.len()),
            Err(error) => {
                problems.push(format!(
                    "{}: could not count the entries not yet sent: {error}",
                    destination.describe()
                ));
                None
            }
        };
    }
    Ok(Swept::Ran {
        tallies,
        problems,
        notes: ignored.clone(),
        waited,
    })
}

/// One destination's pull, then commit and push, under its transport lock;
/// each failure as a line naming the destination.
///
/// The pull goes first, so a remote holding this machine's ref for a
/// checkout that never committed as it is seen, and the commit refused,
/// rather than a second history started and its push rejected. A failed
/// pull does not stop the commit and push: this machine's entries leave
/// whether or not a peer's ref can be read. Neither does a peer entry the
/// pull could not read, which is kept in the tally rather than failing the
/// pass: a destination that failed is not passed over again within the
/// sweep, so an entry this machine wrote meanwhile would wait for the next.
fn pass(destination: &Destination, tally: &mut Tally) -> Vec<String> {
    let failed = |error: SweepError| format!("{}: {error}", destination.describe());
    let _transport = match FileLock::acquire(destination.transport_lock(), LockWait::TRANSPORT) {
        Ok(lock) => lock,
        Err(error) => return vec![failed(error.into())],
    };
    let (done, pulled) = pull_held(destination);
    if let Some(done) = done {
        tally.pulled += done.written;
        tally.unwritten = done
            .unwritten()
            .map(|(_, problem)| format!("{}: {problem}", destination.describe()))
            .collect();
        tally.skipped = Some(done.skipped);
    }
    let mut failures: Vec<String> = pulled.into_iter().map(failed).collect();
    match send(destination, tally) {
        Ok(problems) => failures.extend(
            problems
                .into_iter()
                .map(|problem| format!("{}: {problem}", destination.describe())),
        ),
        Err(error) => failures.push(failed(error)),
    }
    failures
}

/// Fetch every peer's ref and write in what the root lacks; what that
/// wrote, when it could write, and each failure. The caller holds the
/// destination's transport lock.
///
/// A failed fetch still writes in what the copies it fetched carry. git
/// refuses only the ref that would move backward and takes every other
/// machine's, so one peer whose ref was rewritten keeps out that peer's new
/// entries alone, and says so, rather than every machine's, on every pull,
/// until someone repairs it. A fetch that reached nothing leaves the copies
/// as they were, which carry nothing the root lacks.
fn pull_held(destination: &Destination) -> (Option<ledger_git::Materialised>, Vec<SweepError>) {
    let repository = &destination.repository;
    let mut failures = Vec::new();
    let refs = match ledger_git::fetch(repository, REMOTE) {
        Ok(refs) => refs,
        Err(error) => {
            failures.push(error.into());
            match ledger_git::fetched(repository, REMOTE) {
                Ok(refs) => refs,
                Err(error) => {
                    failures.push(error.into());
                    return (None, failures);
                }
            }
        }
    };
    match ledger_git::materialise(repository, &refs) {
        Ok(done) => (Some(done), failures),
        Err(error) => {
            failures.push(error.into());
            (None, failures)
        }
    }
}

/// Commit what is new and push what the remote lacks; what kept the
/// repeated-transition check from reading something, each as a line.
///
/// Before the commit, each transition event this machine wrote that repeats
/// one a ref already carries goes
/// ([`crate::ledger::transitions::repeated_transitions`]): two machines that
/// both synced after one merge each wrote `#N merged`, and the one that
/// sweeps second commits none. Only an entry no ref carries is removed, so
/// what any machine committed stays.
///
/// That check reads every entry of each fork it carries, and what it cannot
/// read holds back nothing else. A fork whose directory cannot be listed is
/// left unchecked, and an entry that does not parse is left out of the
/// check; either is a problem, and the rest is committed and pushed. An
/// entry of this machine's own that does not parse is also left out of the
/// commit: a peer that took it would fail every read of that fork, and a
/// committed entry is never rewritten, while here it can still be fixed.
fn send(destination: &Destination, tally: &mut Tally) -> Result<Vec<String>, SweepError> {
    let repository = &destination.repository;
    let mut problems = Vec::new();
    let mut pending = ledger_git::pending(repository, &destination.machine)?;
    if !pending.added().is_empty() {
        let uncommitted: BTreeSet<PathBuf> = pending
            .added()
            .iter()
            .map(|path| repository.work_tree().join(path))
            .collect();
        for fork in repository.forks() {
            let reads = match Ledger::at(repository.work_tree().join(fork.as_str())).entry_reads() {
                Ok(reads) => reads,
                Err(error) => {
                    problems.push(format!("{fork}: repeated transitions not checked: {error}"));
                    continue;
                }
            };
            let mut entries = Vec::with_capacity(reads.len());
            for (path, read) in reads {
                match read {
                    Ok(entry) => entries.push((path, entry)),
                    Err(error) if uncommitted.contains(&path) => {
                        if let Ok(relative) = path.strip_prefix(repository.work_tree()) {
                            pending.forget(relative);
                        }
                        problems.push(format!(
                            "{error}; not sent until it is fixed or removed, since every \
                             machine that took it would fail to read {fork}'s ledger"
                        ));
                    }
                    Err(error) => problems.push(format!(
                        "{error}; left out of the check for repeated transitions"
                    )),
                }
            }
            for path in crate::ledger::transitions::repeated_transitions(&entries, &uncommitted) {
                if let Err(source) = std::fs::remove_file(path) {
                    problems.push(format!(
                        "{}: a repeated transition, not discarded: {source}",
                        path.display()
                    ));
                    continue;
                }
                if let Ok(relative) = path.strip_prefix(repository.work_tree()) {
                    pending.forget(relative);
                }
                tally.discarded += 1;
                *tally.unkept.entry(fork.to_string()).or_default() += 1;
            }
        }
    }
    if ledger_git::commit_pending(repository, &destination.machine, pending)?.is_some() {
        tally.commits += 1;
    }
    if ledger_git::push(repository, REMOTE, &destination.machine)? {
        tally.pushes += 1;
    }
    Ok(problems)
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
        return remove_if_present(log);
    }
    let mut text = format!("{} the last ledger sweep failed:\n", jiff::Timestamp::now());
    for problem in problems {
        text.push_str(problem);
        text.push('\n');
    }
    std::fs::write(&log, text).map_err(|source| SweepError::Write { path: log, source })
}

/// Remove `path`; one already gone is fine.
fn remove_if_present(path: PathBuf) -> Result<(), SweepError> {
    match std::fs::remove_file(&path) {
        Err(source) if source.kind() != std::io::ErrorKind::NotFound => {
            Err(SweepError::Write { path, source })
        }
        _ => Ok(()),
    }
}

/// Start `knives ledger sweep` detached, and return without waiting for it.
///
/// Called once, as the process exits, by a command that appended an entry
/// ([`crate::ledger::appended`]). The entry is already on disk, so a sweep
/// that never starts loses nothing: the next one carries it. The sweep's
/// output goes nowhere, because the caller has stopped listening by the time
/// it finishes; a sweep that fails says so in [`sweep_log`]. It is marked
/// [`HANDED_OFF`], so it leaves at once when another sweep holds the lock
/// ([`Contended::Leave`]). It runs in a process group of its own, so the
/// Ctrl-C that stops the caller does not stop it mid-push.
///
/// It runs in the config home, which holds the ledger and everything the
/// sweep keeps beside it, rather than in the caller's directory: `finish`
/// run from inside the workspace it removes hands off from a directory that
/// no longer exists, where every git command the sweep runs would fail. The
/// home is named to it as an absolute path, since a relative one would mean
/// another directory from there.
pub fn hand_off() {
    use std::os::unix::process::CommandExt as _;
    let started = std::time::Instant::now();
    let spawned = std::path::absolute(crate::config::default_config_home()).and_then(|home| {
        std::process::Command::new(std::env::current_exe()?)
            .args(["ledger", "sweep"])
            .current_dir(&home)
            .env("KNIVES_CONFIG_HOME", &home)
            .env(HANDED_OFF, "1")
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

    /// The registry beside `root`, as [`registry`] wrote it; empty when none
    /// was written.
    fn loaded(root: &Path) -> Registry {
        crate::config::load(&root.with_file_name("repos.toml")).unwrap()
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
        let missing = destinations(&home.path().join("ledger"), &Registry::default()).unwrap();
        assert!(missing.found.is_empty() && missing.unreached.is_empty());

        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", None)]);
        origin_at(&root, "/nonexistent/remote.git");
        let unshared = destinations(&root, &loaded(&root)).unwrap();
        assert!(unshared.found.is_empty() && unshared.unreached.is_empty());
    }

    #[test]
    fn forks_with_no_machine_name_are_refused_with_the_command_that_names_one() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", Some("acme/ledger"))]);
        origin_at(&root, "/nonexistent/remote.git");

        let error = destinations(&root, &loaded(&root)).unwrap_err().to_string();

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
    fn a_machine_name_no_ref_can_spell_is_refused_with_the_command_that_names_one() {
        // Given: a repository carrying a fork, naming this machine `dev/box`,
        // which would nest under another machine's ref.
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", Some("acme/ledger"))]);
        origin_at(&root, "/nonexistent/remote.git");
        git(&root.join(".git"), &["config", "knives.machine", "dev/box"]);

        // When: where the ledger travels is read, as every sweep and pull does
        // before it fetches.
        let error = destinations(&root, &loaded(&root)).unwrap_err().to_string();

        // Then: it is refused there, naming the name and the command that
        // sets another, rather than failing later mid-sweep.
        assert!(error.contains("\"dev/box\""), "was: {error}");
        assert!(
            error.contains(&format!(
                "git --git-dir={} config knives.machine <name>",
                root.join(".git").display()
            )),
            "was: {error}"
        );
    }

    #[test]
    fn a_knives_key_nothing_reads_is_reported_and_passed_over() {
        // A newer knives may read it, and machines sharing a ledger upgrade
        // at different times: refusing it would stop every pull and sweep.
        // `knives.fork` gets the same note as any other.
        for key in ["knives.forks", "knives.fork"] {
            let (_home, root) = ledger_root();
            git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
            git(&root.join(".git"), &["config", key, "a-repo"]);

            let found = destinations(&root, &loaded(&root)).unwrap();

            assert!(
                matches!(
                    found.ignored.as_slice(),
                    [line] if line.contains(&format!("sets {key},"))
                        && line.contains(&format!("--unset-all {key}"))
                ),
                "{key}: {:?}",
                found.ignored
            );
        }
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

        let found = destinations(&root, &loaded(&root)).unwrap();

        assert!(found.unreached.is_empty(), "{:?}", found.unreached);
        assert_eq!(found.found.len(), 1);
        let destination = found.found.first().unwrap();
        assert_eq!(destination.machine, "alpha");
        assert_eq!(
            destination.repository.forks(),
            &BTreeSet::from([
                UpstreamName::new("acme/a-repo"),
                UpstreamName::new("acme/b-repo")
            ])
        );
    }

    /// A git directory in [`ledger_repositories`] named `name`, over `root`,
    /// whose `origin` is `url`, committing as `machine`.
    fn second_repository(root: &Path, name: &str, url: &str, machine: &str) -> PathBuf {
        let git_dir = ledger_repositories(root).join(name);
        let status = std::process::Command::new("git")
            .args(["init", "--quiet", "--bare"])
            .arg(&git_dir)
            .status()
            .unwrap();
        assert!(status.success());
        git(&git_dir, &["config", "core.bare", "false"]);
        git(&git_dir, &["remote", "add", "origin", url]);
        git(&git_dir, &["config", "knives.machine", machine]);
        git_dir
    }

    #[test]
    fn each_ledger_repository_is_a_destination_carrying_the_forks_that_name_it() {
        // Given: the root's own repository for acme/ledger, a second git
        // directory over the same root for a dot-named personal repository,
        // and a fork naming each.
        let (_home, root) = ledger_root();
        registry(
            &root,
            &[
                ("a-repo", Some("acme/ledger")),
                ("b-repo", Some("someone/.knives-ledger")),
                ("c-repo", None),
            ],
        );
        origin_at(&root, "/nonexistent/remote.git");
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
        let personal = second_repository(
            &root,
            "personal.git",
            "https://forge.invalid/someone/.knives-ledger",
            "alpha-personal",
        );

        // When: the destinations are read.
        let found = destinations(&root, &loaded(&root)).unwrap();

        // Then: there are two, each carrying only the fork that names it,
        // both over the one root, and no fork is unreached.
        assert!(found.unreached.is_empty(), "{:?}", found.unreached);
        let carried: Vec<(PathBuf, String, Vec<String>)> = found
            .found
            .iter()
            .map(|destination| {
                assert_eq!(
                    destination.repository.work_tree(),
                    std::path::absolute(&root).unwrap()
                );
                (
                    destination.repository.git_dir().to_owned(),
                    destination.machine.clone(),
                    destination
                        .repository
                        .forks()
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            carried,
            [
                (
                    std::path::absolute(root.join(".git")).unwrap(),
                    "alpha".to_owned(),
                    vec!["acme/a-repo".to_owned()]
                ),
                (
                    std::path::absolute(&personal).unwrap(),
                    "alpha-personal".to_owned(),
                    vec!["acme/b-repo".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn two_git_directories_with_one_origin_are_refused() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", Some("acme/ledger"))]);
        origin_at(&root, "/nonexistent/remote.git");
        git(&root.join(".git"), &["config", "knives.machine", "alpha"]);
        second_repository(&root, "again.git", LEDGER_URL, "alpha");

        let error = destinations(&root, &loaded(&root)).unwrap_err().to_string();

        assert!(error.contains("again.git"), "was: {error}");
        assert!(error.contains("as its origin, as"), "was: {error}");
    }

    #[test]
    fn a_file_among_the_ledger_repositories_is_refused() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", None)]);
        std::fs::create_dir_all(ledger_repositories(&root)).unwrap();
        std::fs::write(ledger_repositories(&root).join("stray"), "").unwrap();

        let error = destinations(&root, &loaded(&root)).unwrap_err().to_string();

        assert!(error.contains("stray"), "was: {error}");
        assert!(error.contains("is not a git directory"), "was: {error}");
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
            let found = destinations(root, &loaded(root)).unwrap();
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
            let problems = pull_at(root, &loaded(root), &[&a_repo]).problems_for(&a_repo);
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
        let pulled = pull_at(&root, &loaded(&root), &[&a_repo]);

        assert!(pulled.problems_for(&a_repo).is_empty());
        assert!(pulled.backlog().for_fork(&a_repo).is_empty());
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
        let pulled = pull_at(&root, &loaded(&root), &[&a_repo, &b_repo]);

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
