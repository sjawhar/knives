//! Where the ledger travels: the git directories beside it, what each one's
//! own config says, and which forks the registry sends through which.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    pub(super) fn unreached_problem(root: &Path, fork: &UpstreamName, ledger: &str) -> String {
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
pub fn destinations(root: &Path, registry: &Registry) -> Result<Destinations, SweepError> {
    destinations_moving(root, registry, &[])
}

/// [`destinations`], but not refusing the unmigrated directories in `moved`,
/// which the caller is about to move.
pub(super) fn destinations_moving(
    root: &Path,
    registry: &Registry,
    moved: &[&Path],
) -> Result<Destinations, SweepError> {
    refuse_unmigrated(root, registry, moved)?;
    let reached = reach(root, registry)?;
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
        unclaimed: crate::ledger::unclaimed_problems(root, registry),
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
/// setup [`destinations`] refuses, or for a fork no git directory here
/// reaches.
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
