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
use std::sync::OnceLock;

use crate::config::{ConfigError, Registry};
use crate::ids::UpstreamName;
use crate::ledger::Ledger;
use crate::ledger_git::{self, GitError, Repository};
use crate::lock::{FileLock, LockError, LockHolder, LockWait};

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

/// One repository the ledger travels through, and this machine's name there.
#[derive(Debug, Clone)]
pub struct Destination {
    pub repository: Repository,
    pub machine: String,
    /// What every fetch, materialise and commit through this repository holds.
    ///
    /// Two fetches into one repository fail on each other's ref locks, and a
    /// commit that lists the root while a pull writes a peer's entries into
    /// it takes one of them for this machine's own. In the git directory, so
    /// it is outside every fork's directory and no commit can carry it: the
    /// one a gitfile names, when the root's `.git` is one
    /// ([`ledger_git::absolute_git_dir`]).
    transport_lock: PathBuf,
}

impl Destination {
    fn transport_lock(&self) -> &Path {
        &self.transport_lock
    }

    fn carries(&self, fork: &UpstreamName) -> bool {
        self.repository.forks().contains(fork)
    }

    fn describe(&self) -> String {
        format!("{REMOTE} ({})", self.repository.git_dir().display())
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
    /// A problem for each directory under the root holding entries that no
    /// fork is kept under ([`crate::ledger::unclaimed_problems`]): nothing
    /// reads or carries them, so every sweep and every pull says so.
    pub unclaimed: Vec<String>,
    /// One line for each `knives.*` key a git directory sets that this
    /// knives does not read ([`identity`]): a newer knives may, so it is
    /// said, never refused.
    pub ignored: Vec<String>,
}

impl Destinations {
    /// What a fork in [`Destinations::unreached`] is a problem for.
    fn unreached_problem(root: &Path, fork: &UpstreamName, ledger: &str) -> String {
        format!(
            "{fork}: repos.toml says its ledger belongs to {ledger}, but no git directory \
             knives reads ({} and each one in {}) has {ledger} as its origin, so its entries \
             travel to and from no other machine here",
            root.join(".git").display(),
            ledger_repositories(root).display()
        )
    }
}

/// The directory beside the ledger root holding a git directory for each
/// repository the ledger travels through besides the root's own `.git`.
pub fn ledger_repositories(root: &Path) -> PathBuf {
    root.with_file_name("ledger-repositories")
}

/// Every git directory the ledger at `root` may travel through: `<root>/.git`
/// when it exists, then each directory in [`ledger_repositories`] (or
/// symlink to one), in name order. Anything else there is refused: it is no
/// repository, and passing over it would leave a fork carried nowhere
/// without a word.
fn candidates(root: &Path) -> Result<Vec<PathBuf>, SweepError> {
    let mut found = Vec::new();
    let own = root.join(".git");
    if own.try_exists().map_err(|source| SweepError::Read {
        path: own.clone(),
        source,
    })? {
        found.push(own);
    }
    let directory = ledger_repositories(root);
    let unreadable = |path: &Path| {
        let path = path.to_owned();
        move |source| SweepError::Read { path, source }
    };
    let listing = match std::fs::read_dir(&directory) {
        Ok(listing) => listing,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(source) => return Err(unreadable(&directory)(source)),
    };
    let mut others = Vec::new();
    for dirent in listing {
        let path = dirent.map_err(unreadable(&directory))?.path();
        if !std::fs::metadata(&path)
            .map_err(unreadable(&path))?
            .is_dir()
        {
            return Err(SweepError::Config {
                git_dir: path,
                detail: format!(
                    "is not a git directory, and {} holds only the git directories the ledger \
                     travels through",
                    directory.display()
                ),
            });
        }
        others.push(path);
    }
    others.sort();
    found.extend(others);
    Ok(found)
}

/// What a git directory's own config says: `knives.machine`,
/// `remote.origin.url`, and a line for each `knives.*` key this knives does
/// not read.
struct Identity {
    machine: Option<String>,
    origin: Option<String>,
    ignored: Vec<String>,
}

/// [`Identity`] of the git directory `git_dir`.
///
/// Refused, with the command that fixes it: a machine name that cannot spell
/// this machine's ref (`refs/knives/<machine>`) as one component, found
/// otherwise only when a sweep commits and failing there instead. Any other
/// `knives.*` key is one a newer knives may read: it is reported and passed
/// over, since machines sharing a ledger, and sessions on one machine, run
/// different versions.
fn identity(git_dir: &Path) -> Result<Identity, SweepError> {
    let mut found = Identity {
        machine: None,
        origin: None,
        ignored: Vec::new(),
    };
    let pattern = format!("^(knives\\..*|remote\\.{REMOTE}\\.url)$");
    let origin_key = format!("remote.{REMOTE}.url");
    let refused = |detail: String| SweepError::Config {
        git_dir: git_dir.to_owned(),
        detail,
    };
    for (key, value) in ledger_git::local_config(git_dir, &pattern)? {
        match key.as_str() {
            "knives.machine" => {
                if let Err(error) = ledger_git::ref_component("machine", &value) {
                    return Err(refused(format!(
                        "{error}. Name this machine again, uniquely among every machine sharing \
                         {REMOTE}: git --git-dir={} config knives.machine <name>",
                        git_dir.display()
                    )));
                }
                found.machine = Some(value);
            }
            _ if key == origin_key => found.origin = Some(value),
            _ => found.ignored.push(format!(
                "{} sets {key}, which this knives does not read; a newer knives may. If none \
                 here sets it, remove it: git --git-dir={} config --unset-all {key}",
                git_dir.display(),
                git_dir.display()
            )),
        }
    }
    Ok(found)
}

/// Every destination the ledger at `root` travels through.
///
/// The candidates are the repository at `<root>/.git` and each git
/// directory in [`ledger_repositories`], all over the one working tree
/// `root`. Each carries every fork whose `repos.toml` entry, read from
/// beside `root`, names its `origin` as the repository the fork's ledger
/// belongs to ([`crate::config::RepoEntry::ledger`]), and commits as the
/// machine its git config names, `knives.machine`, unique among the
/// machines sharing that remote. The registry is the one source of which
/// forks travel where; a repository's git config says only who this machine
/// is there.
///
/// A fork with no `ledger` is not shared. One whose `ledger` no candidate's
/// `origin` names is [`Destinations::unreached`]. No fork shared and no
/// machine named is a ledger nobody set up to share: no destination, and
/// nothing to report. A candidate naming a machine and carrying no fork is a
/// destination that carries nothing. Refused, each with what fixes it: a
/// machine name no ref can spell ([`identity`]), forks carried with no machine
/// name, two candidates whose `origin` is one repository, since which of
/// them a fork's entries go through would be a guess, and a fork whose
/// registry key's directory still holds the entries an older knives filed
/// there ([`crate::ledger::LedgerError::FormerName`]), which no sweep would
/// carry until `knives ledger migrate` moves them.
pub fn destinations(root: &Path) -> Result<Destinations, SweepError> {
    let registry = crate::config::load(&root.with_file_name("repos.toml"))?;
    refuse_unmigrated(root, &registry, &[])?;
    let reached = reach(root, &registry)?;
    let found = reached
        .carriers
        .into_iter()
        .map(|carrier| {
            Ok(Destination {
                repository: Repository::new(&carrier.git_dir, root, carrier.carried)?,
                transport_lock: ledger_git::absolute_git_dir(&carrier.git_dir)?
                    .join("knives-transport"),
                machine: carrier.machine,
            })
        })
        .collect::<Result<_, SweepError>>()?;
    Ok(Destinations {
        found,
        unreached: reached.unreached,
        unclaimed: crate::ledger::unclaimed_problems(root, &registry),
        ignored: reached.ignored,
    })
}

/// Refuse the ledger at `root` while a fork's registry key's directory
/// still holds the entries an older knives filed there, unless it is one of
/// `moved`, which the caller is about to move.
fn refuse_unmigrated(root: &Path, registry: &Registry, moved: &[&Path]) -> Result<(), SweepError> {
    let former_names = registry.former_names();
    match crate::ledger::unmigrated(root, &former_names)
        .find(|(former, _)| !moved.contains(&root.join(former).as_path()))
    {
        Some((former, now)) => Err(crate::ledger::LedgerError::FormerName {
            path: root.join(now.as_str()),
            former: root.join(former),
        }
        .into()),
        None => Ok(()),
    }
}

/// A candidate git directory that names this machine, with the forks it
/// carries.
struct Carrier {
    git_dir: PathBuf,
    machine: String,
    carried: Vec<UpstreamName>,
}

/// What [`reach`] found.
struct Reached {
    carriers: Vec<Carrier>,
    /// [`Destinations::unreached`].
    unreached: BTreeMap<UpstreamName, String>,
    /// [`Destinations::ignored`].
    ignored: Vec<String>,
}

/// Which candidate of [`destinations`] carries which forks, which forks no
/// candidate reaches ([`Destinations::unreached`]), and the `knives.*` keys
/// passed over, with every refusal [`destinations`] makes but the
/// unmigrated directory's.
///
/// Read from the registry and each candidate's own git config alone: no
/// fetch, and nothing written, not even the ledger root a [`Repository`]
/// creates.
fn reach(root: &Path, registry: &Registry) -> Result<Reached, SweepError> {
    let mut carriers = Vec::new();
    let mut ignored = Vec::new();
    let mut carried_anywhere = BTreeSet::new();
    let mut origins: BTreeMap<String, PathBuf> = BTreeMap::new();
    for git_dir in candidates(root)? {
        let refused = |detail: String| SweepError::Config {
            git_dir: git_dir.clone(),
            detail,
        };
        let found = identity(&git_dir)?;
        ignored.extend(found.ignored);
        let slug = found
            .origin
            .as_deref()
            .and_then(crate::remote_url::remote_slug);
        let kept = slug.map(crate::config::repository_slug);
        if let (Some(slug), Some(kept)) = (slug, &kept)
            && let Some(first) = origins.insert(kept.clone(), git_dir.clone())
        {
            return Err(refused(format!(
                "has {slug} as its origin, as {} does, so which of them carries its forks would \
                 be a guess. Keep one",
                first.display()
            )));
        }
        let carried: Vec<UpstreamName> = registry
            .repos
            .iter()
            .filter(|(_, entry)| {
                kept.as_deref()
                    .is_some_and(|kept| entry.shares_ledger_with(kept))
            })
            .map(|(key, entry)| entry.upstream_name(key))
            .collect();
        match found.machine {
            Some(machine) => {
                carried_anywhere.extend(carried.iter().cloned());
                carriers.push(Carrier {
                    git_dir,
                    machine,
                    carried,
                });
            }
            None if carried.is_empty() => {}
            None => {
                let names: Vec<String> = carried.iter().map(ToString::to_string).collect();
                return Err(refused(format!(
                    "carries the ledgers of {}, but names no machine, so nothing can be \
                     committed as this one. Name this machine, uniquely among every machine \
                     sharing {REMOTE}: git --git-dir={} config knives.machine <name>",
                    names.join(", "),
                    git_dir.display()
                )));
            }
        }
    }
    let unreached = registry
        .repos
        .iter()
        .filter_map(|(key, entry)| {
            let ledger = entry.ledger.as_ref()?;
            let fork = entry.upstream_name(key);
            (!carried_anywhere.contains(&fork)).then(|| (fork, ledger.clone()))
        })
        .collect();
    Ok(Reached {
        carriers,
        unreached,
        ignored,
    })
}

/// What [`pull_at`] would find wrong with `forks` before it fetched.
///
/// For each of them, the problem [`Pulled::failed_for`] would give for a
/// setup [`destinations`] refuses, or for a fork no git directory here
/// reaches.
///
/// Read from the registry and each git directory's own config alone, with
/// no fetch and nothing written, for a dry run that must say what a pull
/// would hold back. `moved` are the directories an older knives filed
/// entries in that the caller would move first, so they are not refused as
/// unmigrated.
pub fn unpullable(
    root: &Path,
    forks: &[&UpstreamName],
    moved: &[&Path],
) -> BTreeMap<UpstreamName, Vec<String>> {
    if forks.is_empty() {
        return BTreeMap::new();
    }
    let reached = crate::config::load(&root.with_file_name("repos.toml"))
        .map_err(SweepError::from)
        .and_then(|registry| {
            refuse_unmigrated(root, &registry, moved)?;
            let reached = reach(root, &registry)?;
            for carrier in &reached.carriers {
                ledger_git::absolute_git_dir(&carrier.git_dir)?;
            }
            Ok(reached.unreached)
        });
    setup_problems(root, forks, reached.as_ref())
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

/// What sweeps did to this machine's ledger that `status` reports, since a
/// sweep a write hands off has nowhere to say it ([`Pulled::backlog_for`]).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Kept {
    /// By fork: the repeated transitions every sweep so far has discarded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    discarded: BTreeMap<String, Discards>,
    /// By fork directory, then by destination: the entries other machines
    /// sent through that destination that this machine does not read, as
    /// the last sweep to pull from it found them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    skipped: BTreeMap<String, BTreeMap<String, usize>>,
}

/// How many repeated transitions of one fork sweeps have discarded, and when
/// the newest went.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Discards {
    count: usize,
    last: String,
}

impl Kept {
    /// What `status` says about `fork` from this record.
    fn notes_for(&self, fork: &UpstreamName) -> Vec<String> {
        let mut notes: Vec<String> = self
            .skipped
            .get(fork.as_str())
            .into_iter()
            .flatten()
            .map(|(through, &count)| {
                let (entries, are) = if count == 1 {
                    ("entry", "is")
                } else {
                    ("entries", "are")
                };
                format!(
                    "{count} {entries} other machines sent through {through} {are} not read \
                     here: this machine's repos.toml does not send {fork}'s ledger there"
                )
            })
            .collect();
        if let Some(discards) = self.discarded.get(fork.as_str()) {
            notes.push(format!(
                "sweeps here discarded {} repeated transition{} another machine had already \
                 committed, the newest at {}",
                discards.count,
                if discards.count == 1 { "" } else { "s" },
                discards.last
            ));
        }
        notes
    }

    /// The [`sweep_record`] beside `root`; empty when there is none.
    fn read(root: &Path) -> Result<Self, SweepError> {
        let path = sweep_record(root);
        match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|source| SweepError::Record { path, source })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SweepError::Read { path, source }),
        }
    }

    /// Replace the [`sweep_record`] beside `root` whole, or remove it when
    /// there is nothing to keep.
    fn write(&self, root: &Path) -> Result<(), SweepError> {
        let path = sweep_record(root);
        let unwritable = |source| SweepError::Write {
            path: path.clone(),
            source,
        };
        if self.discarded.is_empty() && self.skipped.is_empty() {
            return remove_if_present(path);
        }
        let directory = path.parent().unwrap_or(root);
        let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(unwritable)?;
        serde_json::to_writer_pretty(&mut temporary, self)
            .map_err(|error| unwritable(std::io::Error::other(error)))?;
        temporary
            .persist(&path)
            .map_err(|error| unwritable(error.error))?;
        Ok(())
    }

    /// Take `skipped`, what a pull through `through` just found, as all that
    /// is skipped through it now.
    fn pulled_through(&mut self, through: &str, skipped: &BTreeMap<String, usize>) {
        for by in self.skipped.values_mut() {
            by.remove(through);
        }
        for (fork, &count) in skipped {
            self.skipped
                .entry(fork.clone())
                .or_default()
                .insert(through.to_owned(), count);
        }
        self.skipped.retain(|_, by| !by.is_empty());
    }
}

/// Fold what the passes since the last call did into the [`sweep_record`]
/// beside `root`: each tally's discards are added to its fork's, and what
/// each destination's pull found skipped replaces what was kept for it; a
/// destination no longer here is dropped. Called with the lock held, so no
/// other sweep's fold is lost. A discard stays in its tally to fold again
/// until the record holding it is written.
fn keep(
    root: &Path,
    destinations: &[Destination],
    tallies: &mut [Tally],
) -> Result<(), SweepError> {
    let mut kept = Kept::read(root)?;
    let now = jiff::Timestamp::now().to_string();
    let mut present = BTreeSet::new();
    for (destination, tally) in destinations.iter().zip(tallies.iter()) {
        let through = destination.describe();
        for (fork, &count) in &tally.unkept {
            let discards = kept.discarded.entry(fork.clone()).or_insert(Discards {
                count: 0,
                last: String::new(),
            });
            discards.count += count;
            discards.last.clone_from(&now);
        }
        if let Some(skipped) = &tally.skipped {
            kept.pulled_through(&through, skipped);
        }
        present.insert(through);
    }
    for by in kept.skipped.values_mut() {
        by.retain(|through, _| present.contains(through));
    }
    kept.skipped.retain(|_, by| !by.is_empty());
    kept.write(root)?;
    for tally in tallies {
        tally.unkept.clear();
    }
    Ok(())
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

/// Sweep the ledger at `root` through every destination it has, doing what
/// `contended` says when another sweep holds the lock.
///
/// A destination that cannot be read is a failed sweep: it is written to the
/// [`sweep_log`] when no other sweep holds the lock, and returned.
pub fn run(root: &Path, contended: Contended) -> Result<Swept, SweepError> {
    match destinations(root) {
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
/// ([`crate::commands::sync::repeated_transitions`]): two machines that
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
            for path in crate::commands::sync::repeated_transitions(&entries, &uncommitted) {
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

/// What a destination has that its remote does not yet
/// ([`ledger_git::unsent`]), worked out for the whole destination at the
/// first report that asks, or why it could not be.
type Unsent = OnceLock<Result<BTreeSet<PathBuf>, String>>;

/// What pulling the ledger before a decision found, per fork asked about.
#[derive(Debug, Default)]
pub struct Pulled {
    root: PathBuf,
    /// Each destination pulled through, beside its [`Unsent`].
    destinations: Vec<(Destination, Unsent)>,
    problems: BTreeMap<UpstreamName, Vec<String>>,
    /// [`Destinations::unclaimed`]: a problem for every fork asked about.
    unclaimed: Vec<String>,
    /// [`Destinations::ignored`]: a note for every fork asked about.
    ignored: Vec<String>,
    /// By destination, what each pull that succeeded left unread
    /// ([`ledger_git::Materialised::skipped`]).
    skipped: BTreeMap<String, BTreeMap<String, usize>>,
    /// The [`sweep_record`] brought up to date by `skipped`, read at the
    /// first report that asks.
    kept: OnceLock<Result<Kept, String>>,
}

impl Pulled {
    /// The problems a report about `fork` carries: a pull that failed,
    /// destinations that could not be read, and directories of entries no
    /// fork is kept under.
    pub fn problems_for(&self, fork: &UpstreamName) -> Vec<String> {
        self.failed_for(fork)
            .into_iter()
            .chain(self.unclaimed.iter().cloned())
            .collect()
    }

    /// [`Pulled::problems_for`] without the unclaimed directories: why
    /// `fork`'s ledger here may lack what another machine has.
    pub fn failed_for(&self, fork: &UpstreamName) -> Vec<String> {
        self.problems.get(fork).cloned().unwrap_or_default()
    }

    /// What `status` notes about `fork`'s ledger beyond its problems: how
    /// many of its entries its remote does not have yet, and where the last
    /// sweep's failures are when it failed, as one line; then what no one
    /// running a handed-off sweep sees, kept in the [`sweep_record`] and
    /// brought up to date by this pull: entries other machines sent through
    /// a repository this machine does not send the fork through, and
    /// repeated transitions discarded; then each `knives.*` key a git
    /// directory sets that this knives passed over.
    pub fn backlog_for(&self, fork: &UpstreamName) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some((destination, unsent)) = self
            .destinations
            .iter()
            .find(|(destination, _)| destination.carries(fork))
        {
            let unsent = unsent
                .get_or_init(|| {
                    ledger_git::unsent(&destination.repository, REMOTE)
                        .map_err(|error| error.to_string())
                })
                .as_ref()
                .map(|paths| {
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
            notes.extend(match (waiting, failed) {
                (Some(waiting), Some(failed)) => Some(format!("{waiting}; {failed}")),
                (waiting, failed) => waiting.or(failed),
            });
        }
        let kept = self.kept.get_or_init(|| {
            let mut kept = Kept::read(&self.root).map_err(|error| error.to_string())?;
            for (through, skipped) in &self.skipped {
                kept.pulled_through(through, skipped);
            }
            Ok(kept)
        });
        match kept {
            Ok(kept) => notes.extend(kept.notes_for(fork)),
            Err(error) => notes.push(error.clone()),
        }
        notes.extend(self.ignored.iter().cloned());
        notes
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
/// is each peer entry the pull did not write, because it could not read it
/// or its contents disagree ([`ledger_git::Materialised::unwritten`]), for
/// the fork it is in; a fork
/// asked about whose registry entry names a repository its ledger belongs
/// to that no destination here is ([`Destinations::unreached`]); and every
/// directory of entries no fork is kept under ([`Destinations::unclaimed`]),
/// which is a problem for each fork asked about: any of them may be the one
/// reading its ledger without them.
pub fn pull_at(root: &Path, forks: &[&UpstreamName]) -> Pulled {
    let mut pulled = Pulled {
        root: root.to_owned(),
        ..Pulled::default()
    };
    if forks.is_empty() {
        return pulled;
    }
    let reached = destinations(root);
    pulled.problems = setup_problems(
        root,
        forks,
        reached.as_ref().map(|reached| &reached.unreached),
    );
    let Ok(Destinations {
        found,
        unclaimed,
        ignored,
        ..
    }) = reached
    else {
        return pulled;
    };
    pulled.unclaimed = unclaimed;
    pulled.ignored = ignored;
    let asked: BTreeSet<&UpstreamName> = forks.iter().copied().collect();
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
        let (done, failures) = pull_one(&destination);
        if let Some(done) = done {
            for (path, problem) in done.unwritten() {
                if let Some(fork) = carried.iter().find(|fork| path.starts_with(fork.as_str())) {
                    pulled
                        .problems
                        .entry((*fork).clone())
                        .or_default()
                        .push(format!("{}: {problem}", destination.describe()));
                }
            }
            pulled.skipped.insert(destination.describe(), done.skipped);
        }
        for error in failures {
            let problem = format!(
                "could not pull the ledger from {}: {error}; this answers from the entries \
                 this machine has",
                destination.describe()
            );
            for fork in &carried {
                pulled
                    .problems
                    .entry((*fork).clone())
                    .or_default()
                    .push(problem.clone());
            }
        }
        pulled.destinations.push((destination, OnceLock::new()));
    }
    pulled
}

/// [`pull_held`] under the destination's transport lock.
fn pull_one(destination: &Destination) -> (Option<ledger_git::Materialised>, Vec<SweepError>) {
    match FileLock::acquire(destination.transport_lock(), LockWait::TRANSPORT) {
        Ok(_transport) => pull_held(destination),
        Err(error) => (None, vec![error.into()]),
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
pub fn hand_off() {
    use std::os::unix::process::CommandExt as _;
    let started = std::time::Instant::now();
    let spawned = std::env::current_exe().and_then(|executable| {
        std::process::Command::new(executable)
            .args(["ledger", "sweep"])
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
    fn a_machine_name_no_ref_can_spell_is_refused_with_the_command_that_names_one() {
        // Given: a repository carrying a fork, naming this machine `dev/box`,
        // which would nest under another machine's ref.
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", Some("acme/ledger"))]);
        origin_at(&root, "/nonexistent/remote.git");
        git(&root.join(".git"), &["config", "knives.machine", "dev/box"]);

        // When: where the ledger travels is read, as every sweep and pull does
        // before it fetches.
        let error = destinations(&root).unwrap_err().to_string();

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

            let found = destinations(&root).unwrap();

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

        let found = destinations(&root).unwrap();

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
        let found = destinations(&root).unwrap();

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

        let error = destinations(&root).unwrap_err().to_string();

        assert!(error.contains("again.git"), "was: {error}");
        assert!(error.contains("as its origin, as"), "was: {error}");
    }

    #[test]
    fn a_file_among_the_ledger_repositories_is_refused() {
        let (_home, root) = ledger_root();
        registry(&root, &[("a-repo", None)]);
        std::fs::create_dir_all(ledger_repositories(&root)).unwrap();
        std::fs::write(ledger_repositories(&root).join("stray"), "").unwrap();

        let error = destinations(&root).unwrap_err().to_string();

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
        assert!(pulled.backlog_for(&a_repo).is_empty());
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
