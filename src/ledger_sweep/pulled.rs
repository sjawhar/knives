//! The pull a command runs before it decides something from the ledger, and
//! what it found for each fork asked about.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::kept::Kept;
use super::{
    Destination, Destinations, REMOTE, SweepError, destinations, pull_held, setup_problems,
    sweep_log,
};
use crate::config::Registry;
use crate::ids::UpstreamName;
use crate::ledger_git;
use crate::lock::{FileLock, LockWait};

/// What pulling the ledger before a decision found, per fork asked about.
#[derive(Debug, Default)]
pub struct Pulled {
    root: PathBuf,
    /// Each destination pulled through.
    destinations: Vec<Destination>,
    problems: BTreeMap<UpstreamName, Vec<String>>,
    /// [`Destinations::unclaimed`]: a problem for every fork asked about.
    unclaimed: Vec<String>,
    /// [`Destinations::ignored`]: a note for every fork asked about.
    ignored: Vec<String>,
    /// By destination, what each pull that succeeded left unread
    /// ([`ledger_git::Materialised::skipped`]).
    skipped: BTreeMap<String, BTreeMap<String, usize>>,
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

    /// [`Pulled::problems_for`], then each problem the read of `fork`'s
    /// ledger into `store` found ([`crate::store::Store::ledger_problems`]):
    /// what a command that reports from the store adds to its report.
    pub fn problems_with_store(
        &self,
        store: &crate::store::Store,
        fork: &UpstreamName,
    ) -> Vec<String> {
        let mut problems = self.problems_for(fork);
        problems.extend_from_slice(store.ledger_problems(fork));
        problems
    }

    /// What `status` notes about each fork's ledger beyond its problems,
    /// worked out once for the whole pull: what each destination has that
    /// its remote does not yet ([`ledger_git::unsent`]), and the
    /// [`sweep_record`] brought up to date by this pull ([`Backlog::for_fork`]).
    pub fn backlog(&self) -> Backlog<'_> {
        let unsent = self
            .destinations
            .iter()
            .map(|destination| {
                let unsent = ledger_git::unsent(&destination.repository, REMOTE)
                    .map_err(|error| error.to_string());
                (destination, unsent)
            })
            .collect();
        let kept = Kept::read(&self.root)
            .map(|mut kept| {
                for (through, skipped) in &self.skipped {
                    kept.pulled_through(through, skipped);
                }
                kept
            })
            .map_err(|error| error.to_string());
        Backlog {
            pulled: self,
            unsent,
            kept,
        }
    }
}

/// What a pull left to say about each fork's ledger beyond its problems
/// ([`Pulled::backlog`]).
#[derive(Debug)]
pub struct Backlog<'a> {
    pulled: &'a Pulled,
    /// Each destination pulled through, with what it has that its remote
    /// does not yet, or why that could not be counted.
    unsent: Vec<(&'a Destination, Result<BTreeSet<PathBuf>, String>)>,
    /// The [`sweep_record`] brought up to date by the pull's `skipped`, or
    /// why it could not be read.
    kept: Result<Kept, String>,
}

impl Backlog<'_> {
    /// What `status` notes about `fork`'s ledger beyond its problems: how
    /// many of its entries its remote does not have yet, and where the last
    /// sweep's failures are when it failed, as one line; then what no one
    /// running a handed-off sweep sees, kept in the [`sweep_record`] and
    /// brought up to date by this pull: entries other machines sent through
    /// a repository this machine does not send the fork through, and
    /// repeated transitions discarded; then each `knives.*` key a git
    /// directory sets that this knives passed over.
    pub fn for_fork(&self, fork: &UpstreamName) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some((destination, unsent)) = self
            .unsent
            .iter()
            .find(|(destination, _)| destination.carries(fork))
        {
            let unsent = unsent.as_ref().map(|paths| {
                paths
                    .iter()
                    .filter(|path| path.starts_with(fork.as_str()))
                    .count()
            });
            let log = sweep_log(&self.pulled.root);
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
        match &self.kept {
            Ok(kept) => notes.extend(kept.notes_for(fork)),
            Err(error) => notes.push(error.clone()),
        }
        notes.extend(self.pulled.ignored.iter().cloned());
        notes
    }
}

/// Pull the ledgers of `forks` from every destination `registry` gives one,
/// before a command decides something from them.
pub fn pull(registry: &Registry, forks: &[&UpstreamName]) -> Pulled {
    pull_at(&crate::ledger::default_ledger_root(), registry, forks)
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
pub fn pull_at(root: &Path, registry: &Registry, forks: &[&UpstreamName]) -> Pulled {
    let mut pulled = Pulled {
        root: root.to_owned(),
        ..Pulled::default()
    };
    if forks.is_empty() {
        return pulled;
    }
    let reached = destinations(root, registry);
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
        pulled.destinations.push(destination);
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
