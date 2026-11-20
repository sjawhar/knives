//! Where the ledger travels: the git directories beside it, what each one's
//! own config says, and which forks the registry sends through which.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::join::{join_unreached, joined_git_dir, occupied};
use super::{REMOTE, SweepError, setup_problems};
use crate::config::Registry;
use crate::ids::UpstreamName;
use crate::ledger_git::{self, Repository};

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
    pub(super) fn transport_lock(&self) -> &Path {
        &self.transport_lock
    }

    pub(super) fn carries(&self, fork: &UpstreamName) -> bool {
        self.repository.forks().contains(fork)
    }

    pub(super) fn describe(&self) -> String {
        format!("{REMOTE} ({})", self.repository.git_dir().display())
    }
}

/// Where the ledger at `root` travels, as the registry beside it says.
#[derive(Debug, Default)]
pub struct Destinations {
    /// The repositories it travels through.
    pub found: Vec<Destination>,
    /// Each fork whose registry entry names a repository its ledger belongs
    /// to that no destination here is: its entries travel to and from no
    /// other machine from this one. For a read that routes forks
    /// ([`destinations_telling`]), it holds only the routed forks. Outside a
    /// dry run, only a fork whose ledger could not be joined.
    pub unreached: BTreeMap<UpstreamName, Unreached>,
    /// A problem for each directory under the root holding entries that no
    /// fork is kept under ([`crate::ledger::unclaimed_problems`]): nothing
    /// reads or carries them, so every sweep and every pull says so.
    pub unclaimed: Vec<String>,
    /// One line for each `knives.*` key a git directory sets that this
    /// knives does not read ([`identity`]): a newer knives may, so it is
    /// said, never refused.
    pub ignored: Vec<String>,
}

/// A fork in [`Destinations::unreached`]: the ledger its registry entry
/// names, and why no git directory here carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreached {
    /// The `<owner>/<name>` of the repository its ledger belongs to.
    pub ledger: String,
    /// Why joining that ledger failed; `None` where nothing tried to join
    /// it: under a dry run, which writes nothing.
    pub join_failed: Option<String>,
}

impl Destinations {
    /// What a fork in [`Destinations::unreached`] is a problem for.
    pub(super) fn unreached_problem(
        root: &Path,
        fork: &UpstreamName,
        unreached: &Unreached,
    ) -> String {
        let Unreached {
            ledger,
            join_failed,
        } = unreached;
        let unreached = format!(
            "{fork}: repos.toml says its ledger belongs to {ledger}, but no git directory \
             knives reads ({} and each one in {}) has {ledger} as its origin",
            root.join(".git").display(),
            ledger_repositories(root).display()
        );
        join_failed.as_ref().map_or_else(
            || {
                format!(
                    "{unreached} yet; a run that is not a dry run joins it first, making {}, \
                     and then pulls",
                    joined_git_dir(root, ledger).display()
                )
            },
            |why| {
                format!(
                    "{unreached}, and joining it failed: {why}. Until it is joined, its \
                     entries travel to and from no other machine here"
                )
            },
        )
    }

    /// Why no destination here carries `fork`, whose registry entry names
    /// `ledger`: as [`Destinations::unreached`] says, or, where it says
    /// nothing, that the git directory joining that ledger would make is
    /// another repository's.
    pub(super) fn uncarried_problem(
        &self,
        root: &Path,
        fork: &UpstreamName,
        ledger: &str,
    ) -> String {
        let unreached = self.unreached.get(fork).map_or_else(
            || {
                std::borrow::Cow::Owned(Unreached {
                    ledger: ledger.to_owned(),
                    join_failed: Some(occupied(root, ledger)),
                })
            },
            std::borrow::Cow::Borrowed,
        );
        Self::unreached_problem(root, fork, &unreached)
    }
}

/// The destination that carries one shared fork ([`shared_fork`]), the
/// ledger that fork's registry entry names, and the ledger root.
#[derive(Debug, Clone)]
pub struct SharedFork {
    /// The `<owner>/<name>` of the repository the fork's ledger belongs to.
    pub ledger: String,
    pub destination: Destination,
    /// The ledger root, beside which the ledger's lock is.
    pub root: PathBuf,
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
pub(super) struct Identity {
    machine: Option<String>,
    pub(super) origin: Option<String>,
    ignored: Vec<String>,
}

/// [`Identity`] of the git directory `git_dir`.
///
/// Refused, with the command that fixes it: a machine name that cannot spell
/// this machine's ref (`refs/knives/<machine>`) as one component, or spells
/// one longer than the ledger's remote holds
/// ([`ledger_git::machine_component`]), found otherwise only when a sweep
/// commits and failing there instead. Any other `knives.*` key is one a newer
/// knives may read: it is reported and passed over, since machines sharing a
/// ledger, and sessions on one machine, run different versions.
pub(super) fn identity(git_dir: &Path) -> Result<Identity, SweepError> {
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
                if let Err(error) = ledger_git::machine_component(&value) {
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

/// Every destination the ledger at `root` travels through, as `registry`
/// says.
///
/// The candidates are the repository at `<root>/.git` and each git
/// directory in [`ledger_repositories`], all over the one working tree
/// `root`. Each carries every fork whose `registry` entry names its `origin`
/// as the repository the fork's ledger belongs to
/// ([`crate::config::RepoEntry::ledger`]), and commits as the machine its
/// git config names, `knives.machine`, unique among the machines sharing
/// that remote. The registry is the one source of which forks travel where;
/// a repository's git config says only who this machine is there.
///
/// A fork with no `ledger` is not shared. A ledger no candidate's `origin`
/// names is joined first ([`super::join`]): its git directory is made in
/// [`ledger_repositories`], said in one line on stderr, and the candidates
/// are read again, so that it is one of them. Every such ledger the registry
/// names is joined, for a sweep. A fork whose ledger could not be
/// joined is [`Destinations::unreached`], with why. No fork shared and no
/// machine named is a ledger nobody set up to share: no destination, and
/// nothing to report. A candidate naming a machine and carrying no fork is a
/// destination that carries nothing. Refused, each with what fixes it: a
/// machine name no ref the ledger's remote holds can spell ([`identity`]),
/// forks carried with no machine name, two candidates whose `origin` is one
/// repository, since which of them a fork's entries go through would be a
/// guess, and a fork whose registry key's directory still holds the entries
/// an older knives filed there ([`crate::ledger::LedgerError::FormerName`]),
/// which no sweep would carry until `knives ledger migrate` moves them.
pub fn destinations(root: &Path, registry: &Registry) -> Result<Destinations, SweepError> {
    destinations_telling(root, registry, None, &mut std::io::stderr())
}

/// [`destinations`], saying each ledger it joins on `said`, and joining
/// only the ledgers of `routed`, when it names the forks the caller routes.
///
/// A command that routes one fork joins that fork's ledger alone. A ledger
/// that cannot be joined is tried again on every read, so joining the
/// ledger of a fork it does not route would make it wait on that one, up to
/// the transport deadline, while it holds its own fork's lock. Its
/// [`Destinations::unreached`] names only forks of `routed`: of another
/// fork's ledger, which it did not try to join, it has nothing true to say.
pub(super) fn destinations_telling(
    root: &Path,
    registry: &Registry,
    routed: Option<&[&UpstreamName]>,
    said: &mut dyn Write,
) -> Result<Destinations, SweepError> {
    refuse_unmigrated(root, registry, &[])?;
    let mut reached = reach(root, registry)?;
    let joined: BTreeSet<String> = match routed {
        None => reached.unreached.values().cloned().collect(),
        Some(forks) => forks
            .iter()
            .filter_map(|fork| reached.unreached.get(*fork))
            .cloned()
            .collect(),
    };
    let mut failed = BTreeMap::new();
    if !joined.is_empty() {
        failed = join_unreached(
            root,
            registry,
            reached
                .unreached
                .iter()
                .filter(|(_, ledger)| joined.contains(*ledger)),
            said,
        );
        // Only a join that made a git directory, or found one another
        // command made, gives the candidates another to read.
        if failed.len() < joined.len() {
            reached = reach(root, registry)?;
        }
    }
    // A fork it does not route is left out: its entry would say nothing
    // tried to join its ledger, which only a dry run's entries say.
    if let Some(forks) = routed {
        reached.unreached.retain(|fork, _| forks.contains(&fork));
    }
    // A ledger still unreached once joined is one whose directory was
    // already there, its origin another repository's.
    assemble(root, registry, reached, |ledger| {
        Some(
            failed
                .get(ledger)
                .cloned()
                .unwrap_or_else(|| occupied(root, ledger)),
        )
    })
}

/// [`destinations`], but joining no ledger and not refusing the unmigrated
/// directories in `moved`, which the caller is about to move: for a dry
/// run, which writes nothing.
pub(super) fn destinations_moving(
    root: &Path,
    registry: &Registry,
    moved: &[&Path],
) -> Result<Destinations, SweepError> {
    refuse_unmigrated(root, registry, moved)?;
    assemble(root, registry, reach(root, registry)?, |_| None)
}

/// The [`Destinations`] `reached` gives, each unreached fork with why its
/// ledger was not joined, as `join_failed` says of that ledger.
fn assemble(
    root: &Path,
    registry: &Registry,
    reached: Reached,
    join_failed: impl Fn(&str) -> Option<String>,
) -> Result<Destinations, SweepError> {
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
    let unreached = reached
        .unreached
        .into_iter()
        .map(|(fork, ledger)| {
            let join_failed = join_failed(&ledger);
            (
                fork,
                Unreached {
                    ledger,
                    join_failed,
                },
            )
        })
        .collect();
    Ok(Destinations {
        found,
        unreached,
        unclaimed: crate::ledger::unclaimed_problems(root, registry),
        ignored: reached.ignored,
    })
}

/// The destination that carries `fork`, whose registry entry names the
/// ledger `ledger` ([`ledger_of`]), as [`destinations`] finds it.
///
/// The fork's ledger is joined first when no git directory reaches it, and
/// no other ledger is ([`destinations_telling`]). Refused as
/// [`destinations`] refuses, and with [`SweepError::Unreached`] when
/// `fork`'s ledger could not be joined: a command about to decide from, or
/// write to, a shared fork's ledger cannot go on without it.
pub fn shared_fork(
    root: &Path,
    registry: &Registry,
    fork: &UpstreamName,
    ledger: &str,
) -> Result<SharedFork, SweepError> {
    let mut reached = destinations_telling(
        root,
        registry,
        Some(std::slice::from_ref(&fork)),
        &mut std::io::stderr(),
    )?;
    if let Some(index) = reached
        .found
        .iter()
        .position(|destination| destination.carries(fork))
    {
        return Ok(SharedFork {
            ledger: ledger.to_owned(),
            destination: reached.found.swap_remove(index),
            root: root.to_owned(),
        });
    }
    Err(SweepError::Unreached {
        problem: reached.uncarried_problem(root, fork, ledger),
    })
}

/// The `<owner>/<name>` of the ledger `fork`'s registry entry names; `None`
/// when it names none, or `registry` has no entry for `fork`.
pub fn ledger_of<'r>(registry: &'r Registry, fork: &UpstreamName) -> Option<&'r str> {
    registry
        .kept_under(fork)
        .and_then(|entry| entry.ledger.as_deref())
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
    /// Each fork with a ledger no candidate carries, with the `<owner>/<name>`
    /// of that ledger: [`Destinations::unreached`] before any join.
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
/// fetch, and nothing written.
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
/// setup [`destinations`] refuses; and for a fork no git directory here
/// reaches, that a pull would join its ledger first, which a dry run cannot
/// do and so cannot pull it either.
///
/// Read from `registry` and each git directory's own config alone, with no
/// fetch and nothing written, for a dry run that must say what a pull would
/// hold back. `moved` are the directories an older knives filed entries in
/// that the caller would move first, so they are not refused as unmigrated.
pub fn unpullable(
    root: &Path,
    registry: &Registry,
    forks: &[&UpstreamName],
    moved: &[&Path],
) -> BTreeMap<UpstreamName, Vec<String>> {
    if forks.is_empty() {
        return BTreeMap::new();
    }
    let found = destinations_moving(root, registry, moved);
    setup_problems(root, forks, found.as_ref().map(|found| &found.unreached))
}
