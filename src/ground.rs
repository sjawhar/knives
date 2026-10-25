//! What every fork verb stands on: the registry, the current directory bound
//! against it once, and the entries a many-repo verb covers.

use knives::bind::{Fork, Unbound};
use knives::cli::Exit;
use knives::commands::claim::{Bound, current_identity, identity_outside_a_fork};
use knives::config::{ConfigError, NO_HOME, Registry, RepoEntry, default_config_path, home_dir};
use knives::ids::{RepoName, UpstreamName};

/// The scan root, or `None` after saying that there is none (exit `Usage`).
pub(crate) fn scan_home() -> Option<std::path::PathBuf> {
    let home = home_dir();
    if home.is_none() {
        eprintln!("{NO_HOME}");
    }
    home
}

/// The fork verbs' shared step, taken once per invocation by a verb that acts
/// on a fork: the current directory bound against the registry, and the
/// sighting recorded from that bind — one remotes read for both and the verb.
/// The sighting never fails a call; a verb that writes resolves who is acting
/// where it writes, from the same binding ([`Ground::bound`]).
pub(crate) fn grounded(loaded: &Result<Registry, ConfigError>) -> anyhow::Result<Ground<'_>> {
    let cwd = std::env::current_dir();
    let ground = match (&cwd, loaded) {
        (Ok(cwd), Ok(registry)) => Ok(Ground::new(registry, cwd)),
        (Err(error), _) => Err(anyhow::anyhow!("reading the current directory: {error}")),
        (Ok(_), Err(error)) => Err(anyhow::anyhow!("{error}")),
    };
    let identity = ground.as_ref().map_or_else(
        |_| Ok(identity_outside_a_fork()),
        |ground| current_identity(ground.bound()),
    );
    let bound = ground
        .as_ref()
        .ok()
        .and_then(|ground| ground.bound().upstream);
    if let (Ok(cwd), Ok(identity)) = (&cwd, identity) {
        knives::seen::record_observation(bound, cwd, &identity);
    }
    ground
}

/// What every fork verb shares: the registry, and the current directory bound
/// against it once.
pub(crate) struct Ground<'a> {
    pub(crate) registry: &'a Registry,
    /// Exactly what `bind::here` said, so the verb reports a refusal as it
    /// always has, without asking the VCS again.
    here: Result<Fork<'a>, Unbound<'a>>,
}

impl<'a> Ground<'a> {
    fn new(registry: &'a Registry, cwd: &std::path::Path) -> Self {
        Self {
            registry,
            here: knives::bind::here(registry, cwd),
        }
    }

    /// The registry, and the fork the current directory is inside when it
    /// is inside one, by the name its claims are kept under: what
    /// `current_identity` derives a terminal user's name from, and what a
    /// sighting keys its workspace on.
    pub(crate) fn bound(&self) -> Bound<'_> {
        Bound {
            registry: self.registry,
            upstream: self.here.as_ref().ok().map(|fork| &fork.upstream),
        }
    }

    /// The fork `name` is: the current directory's when it is that entry, else
    /// the scan's; `None` after printing why not (exit `Usage`). How the
    /// current directory bound is [`knives::bind::resolve`]'s to weigh.
    fn named(&self, name: &str) -> Option<Fork<'a>> {
        let registry = self.registry;
        let name = RepoName::new(name);
        let home = scan_home()?;
        match knives::bind::resolve(registry, &name, &self.here, &home) {
            Ok(fork) => Some(fork),
            Err(why) => {
                eprintln!("{}", why.message(&name, registry));
                None
            }
        }
    }

    /// The single fork a verb acts on: named, or the one the current directory
    /// is inside; `None` after printing why not (exit `Usage`). A current
    /// directory whose remotes cannot be read is an error (exit `Incomplete`).
    ///
    /// Requiring the name on every command is absurd when you are inside the
    /// repository, and it was the loudest complaint about using this thing.
    pub(crate) fn one_fork(&self, requested: Option<&str>) -> anyhow::Result<Option<Fork<'a>>> {
        if let Some(name) = requested {
            return Ok(self.named(name));
        }
        match &self.here {
            Ok(fork) => Ok(Some(fork.clone())),
            Err(Unbound::Unreadable(error)) => Err(error.clone().into()),
            Err(unbound) => {
                eprintln!("{}", unbound.message(self.registry));
                Ok(None)
            }
        }
    }

    /// Every entry a many-repo verb covers, bound where the scan could.
    ///
    /// A name wins. Otherwise the repo you are standing in, because that is
    /// nearly always what you meant, and reporting on ten repositories at once
    /// is how `status` became unreadable. `--all` asks for all of them
    /// explicitly, and standing outside every managed repo also means all of
    /// them, since there is nothing else it could mean. Standing in a git clone
    /// is refused as every fork verb refuses it, and a checkout whose remotes
    /// cannot be read is an error, not a reason to sweep. An entry the scan
    /// found twice is still a row — rendered as a problem — and nothing is
    /// opened for it. What the scan could not read is said once, on stderr,
    /// whichever output format the document takes.
    pub(crate) fn selected(
        self,
        scope: Scope<'_>,
    ) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>> {
        let registry = self.registry;
        if registry.is_empty() {
            eprintln!(
                "no repos configured; add entries to {}",
                default_config_path().display()
            );
            return Ok(Err(Exit::Usage));
        }
        if let Some(name) = scope.requested {
            return Ok(self
                .named(name)
                .map_or(Err(Exit::Usage), |fork| Ok(vec![Selected::Bound(fork)])));
        }
        if !scope.all
            && let Some(answer) = standing_row(self.here, registry)
        {
            return answer;
        }
        let Some(home) = scan_home() else {
            return Ok(Err(Exit::Usage));
        };
        let (selected, problems) = sweep(registry, &home);
        for problem in &problems {
            eprintln!("could not read: {problem}");
        }
        Ok(Ok(selected))
    }

    /// `sync` fetches and writes, so it never sweeps by accident: a name,
    /// `--all`, or the fork the current directory is inside; anything else is
    /// `Usage`, except a checkout whose remotes cannot be read, which is an
    /// error.
    pub(crate) fn sync_targets(
        self,
        scope: Scope<'_>,
    ) -> anyhow::Result<Result<Vec<Selected<'a>>, Exit>> {
        if scope.requested.is_some() || scope.all {
            return self.selected(scope);
        }
        standing_row(self.here, self.registry).unwrap_or_else(|| {
            eprintln!("give a repo name, or --all");
            Ok(Err(Exit::Usage))
        })
    }
}

/// The row a many-repo verb reports for the directory it stands in, when
/// how it bound decides one: the fork it is inside; the disagreeing entry,
/// as an unplaced row; a git clone or non-colocated checkout, refused
/// (`Usage`); a checkout whose remotes cannot be read, an error. `None` when
/// standing there names no entry, which each caller answers its own way.
fn standing_row<'a>(
    here: Result<Fork<'a>, Unbound<'a>>,
    registry: &Registry,
) -> Option<anyhow::Result<Result<Vec<Selected<'a>>, Exit>>> {
    match here {
        Ok(fork) => Some(Ok(Ok(vec![Selected::Bound(fork)]))),
        Err(Unbound::Unreadable(error)) => Some(Err(error.into())),
        Err(Unbound::Disagrees(disagreement)) => {
            Some(Ok(Ok(vec![Selected::disagreeing(disagreement)])))
        }
        Err(unbound @ (Unbound::GitOnly { .. } | Unbound::NotColocated { .. })) => {
            eprintln!("{}", unbound.message(registry));
            Some(Ok(Err(Exit::Usage)))
        }
        Err(_) => None,
    }
}

/// One registry entry as a many-repo verb sees it after the scan.
pub(crate) enum Selected<'a> {
    Bound(Fork<'a>),
    /// Found twice, or not found while the scan could not read something that
    /// may have been it: still a row, never opened. `problem` is the refusal;
    /// the scan's own complaints are reported once, not on every row.
    Unplaced {
        name: RepoName,
        entry: &'a RepoEntry,
        problem: String,
    },
}

impl<'a> Selected<'a> {
    /// What the entry's ledger and state are kept under, placed or not.
    pub(crate) fn upstream(&self) -> UpstreamName {
        match self {
            Selected::Bound(fork) => fork.upstream.clone(),
            Selected::Unplaced { name, entry, .. } => entry.upstream_name(name.as_str()),
        }
    }

    /// The row for the entry the current directory's checkout carries the
    /// `origin` of while its `upstream` disagrees: unplaced, never opened,
    /// its problem the disagreement in full.
    fn disagreeing(disagreement: knives::bind::Disagreement<'a>) -> Self {
        Selected::Unplaced {
            problem: disagreement.to_string(),
            name: disagreement.fork,
            entry: disagreement.entry,
        }
    }
}

/// The repositories among `chosen` that a verb will gather, and so whose branch
/// statements it will ask the store about. An unplaced entry is never opened.
pub(crate) fn placed<'s>(chosen: &'s [Selected<'_>]) -> Vec<&'s UpstreamName> {
    chosen
        .iter()
        .filter_map(|chosen| match chosen {
            Selected::Bound(fork) => Some(&fork.upstream),
            Selected::Unplaced { .. } => None,
        })
        .collect()
}

/// Every entry the scan of `home` placed, and what the scan could not read,
/// for the caller to report once: each unplaced row carries only its own
/// refusal, whether or not every entry was found.
///
/// An entry with no checkout under `home` is not on this machine, which is
/// not a problem with it: the registry is shared across machines that hold
/// different subsets. It is noted once on stderr and left out. Unless the
/// scan failed to read something — then the missing checkout may be the one
/// it could not read, and the entry stays a row carrying that refusal.
fn sweep<'a>(registry: &'a Registry, home: &std::path::Path) -> (Vec<Selected<'a>>, Vec<String>) {
    let mut scan = knives::bind::scan(registry, home);
    let selected = registry
        .repos
        .iter()
        .filter_map(|(name, entry)| {
            let name = RepoName::new(name.clone());
            if let Some(fork) = scan.found.remove(&name) {
                return Some(Selected::Bound(fork));
            }
            let why = scan.unplaced(&name, Vec::new());
            if matches!(why, knives::bind::Unresolved::Missing { .. }) && scan.problems.is_empty() {
                eprintln!("knives: {name}: not on this machine");
                return None;
            }
            Some(Selected::Unplaced {
                name: name.clone(),
                entry,
                problem: format!("could not gather: {}", why.message(&name, registry)),
            })
        })
        .collect();
    (selected, scan.problems)
}

/// Which repos a report covers: a named one, all of them, or (neither) the one
/// the current directory is inside.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scope<'a> {
    pub(crate) requested: Option<&'a str>,
    pub(crate) all: bool,
}
