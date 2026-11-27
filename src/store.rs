//! The things no amount of computing can recover.
//!
//! Detectors are cheap and local, so nothing derived is cached here. What lives
//! here is intent: who is working on what and why, and why we carry someone
//! else's pull request as a release parent.
//!
//! What a person stated about a branch — its pull request, that it has none
//! upstream on purpose, what superseded it, what it cannot land before — lives on
//! the ledger entry that recorded the statement. The store reads back the ledgers
//! of the repositories its caller names when it opens, and answers for them, so a
//! reader asks the store either way.
//!
//! Intent cannot be inferred from the repository, and it cannot be inferred from
//! session working directories either: an agent launched elsewhere may need to
//! change a fork.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{FormerNames, default_config_path};
use crate::ids::{BranchName, BranchTarget, CommitId, Requirement, UpstreamName};
use crate::ledger::{Entry, Ledger, LedgerError, inline_human_text, unmigrated};
use crate::lock::{FileLock, LockError, LockWait};
use crate::statement::{StatementKind, Statements};

use crate::commands::claim::Identity;
use crate::commands::sync::ForgeState;

pub mod migration;

use migration::{LEGACY_STATEMENTS, StateMap};

pub fn default_state_path() -> PathBuf {
    default_config_path().with_file_name("state.json")
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnerKind {
    HarnessSession,
    WorkspaceDerived,
    #[default]
    OsUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub repo: String,
    pub branch: String,
    pub owner: String,
    #[serde(default)]
    pub kind: OwnerKind,
    pub why: String,
    pub started: String,
    #[serde(default)]
    pub files: Vec<String>,
    /// Where the claim was taken: host name and PID namespace
    /// ([`crate::liveness::this_place`]). A process in another place cannot see
    /// this one's processes, so `finish` from there cannot tell whether the
    /// workspace is still in use. Absent on claims taken before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// The `knives.machine` of the ledger git directory the claim was taken
    /// through, on a fork that shares its ledger; absent on a claim this
    /// machine keeps to itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// The claim commit on the ledger's remote that this is a copy of.
    ///
    /// On a fork that shares its ledger, the ledger's claim ref says who
    /// holds the branch, on every machine; this machine keeps a copy here so
    /// that `status`, the session hook and preflight, which read this file
    /// alone, still list it. A copy is right only while the claim ref holds
    /// this commit, so it decides nothing. A claim with no commit is a local
    /// claim: one an older knives took, or one taken before the registry
    /// named a ledger, which binds this machine alone, as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_commit: Option<String>,
}

impl Claim {
    pub fn key(&self) -> String {
        format!("{}/{}", self.repo, self.branch)
    }
}

/// On-disk shape.
///
/// `extra` catches every key this version does not know about and writes it back
/// untouched, so an older binary cannot silently delete a newer one's data.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub claims: BTreeMap<String, Claim>,
    #[serde(default)]
    pub foreign_parents: BTreeMap<String, String>,
    #[serde(default)]
    pub pull_heads: BTreeMap<String, BTreeMap<String, String>>,
    /// Digest of each convention file the last time we looked, so preflight can
    /// say "this changed since you last read it" rather than only "it exists".
    #[serde(default)]
    pub conventions: BTreeMap<String, String>,
    #[serde(default)]
    pub comment_marks: BTreeMap<String, String>,
    /// The latest pull-request state sync observed. Keyed by `<repo>#<number>`
    /// so automatic events remain edges instead of repeating settled conditions.
    #[serde(default)]
    pub pull_states: BTreeMap<String, String>,
    /// Keys this version does not know, kept verbatim through a round trip.
    ///
    /// Release membership is the release commit's own parent set, edited by
    /// `release include|drop|advance`, so nothing states it here; whatever an
    /// older version wrote lands in this map and rides along rather than
    /// failing the read. So do the branch statements an older version kept
    /// here, until `knives ledger migrate` moves them onto the ledger
    /// ([`Store::legacy_statements`]) and drops them.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
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
    #[error("{path} is not valid state: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "{} still holds the branch statements an older knives kept there ({maps}); this \
         knives reads them only from the ledger, so it would report every one of them as \
         unstated. Run `knives ledger migrate` once to move them",
        path.display()
    )]
    Unmigrated { path: PathBuf, maps: String },
    #[error(
        "{} still keeps forks under the registry keys an older knives named them by ({held}); \
         this knives keeps a fork's ledger and state under its upstream repository, so it \
         would report their claims, statements and pull request records as absent. Run \
         `knives ledger migrate` once to move them",
        path.display()
    )]
    FormerNames { path: PathBuf, held: String },
    #[error(
        "the claim on {key} changed while this command ran ({now}); nothing was written to {}, \
         but what the command did before (a workspace, a ledger event) stays. Read `knives \
         status`, then run the command again to decide from the claim as it is now",
        path.display()
    )]
    ClaimMoved {
        path: PathBuf,
        key: String,
        now: String,
    },
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("serialising state: {source}")]
    Serialise {
        #[from]
        source: serde_json::Error,
    },
}

/// One change a writer made to the state, replayed onto the file as it is when
/// the store saves ([`Store::save`]).
type Change = Box<dyn Fn(&mut State, &Path) -> Result<(), StoreError> + Send + Sync>;

/// How a store's save reaches the file.
enum Writes {
    /// Each change since the last save, replayed in order onto the file as
    /// it is at the save, under the state file's lock taken for that one
    /// write. Another fork's writer, or a report recording what it saw, may
    /// have saved since this store read the file; a whole-file write from what
    /// this store read would erase what they wrote.
    Replayed(Vec<Change>),
    /// `knives ledger migrate`'s rewrite of the whole file: its renames are
    /// not changes to replay. The state file's lock, held since the open
    /// (none for a dry run, which never saves), keeps every other save out.
    Whole(Option<FileLock>),
}

impl std::fmt::Debug for Writes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replayed(changes) => write!(formatter, "Replayed({} changes)", changes.len()),
            Self::Whole(lock) => formatter.debug_tuple("Whole").field(lock).finish(),
        }
    }
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    state: State,
    /// The ledger of each repository the store was opened for, read once
    /// when the store opens, and again by [`Store::reread_locked`], or why
    /// it could not be read at all: only a store opened with
    /// [`Store::open_each`] keeps one it could not read; every other open
    /// fails on it.
    ledgers: BTreeMap<UpstreamName, Result<ForkLedger, LedgerError>>,
    writes: Writes,
    /// The lock on each fork a store opened for update was opened for
    /// ([`Store::open_for_update`]). Held, and read only for whether there
    /// are any: its whole job is to exist until this value is dropped. A
    /// store that holds them has nothing to take again in
    /// [`Store::reread_locked`].
    forks: Vec<FileLock>,
}

/// What one read of a fork's ledger gave the store ([`Ledger::read`]).
#[derive(Debug)]
struct ForkLedger {
    /// Every entry this knives can read, oldest first: what a report
    /// showing them answers from.
    entries: Vec<Entry>,
    /// The live statements of those entries.
    statements: Statements,
    /// What the read found it could read only in part.
    problems: Vec<String>,
}

impl Store {
    /// Takes no lock to open, so it cannot block another agent. A change
    /// made on it is replayed at [`Store::save`] onto the file as it is then,
    /// so it loses no other writer's change; a writer whose change follows
    /// from what it read must not race another such writer, and opens
    /// [`Store::open_for_update`] instead, holds the one branch's lock its
    /// change is about ([`BranchLock`]: a claim it changes is compared on
    /// save with the one it read), or takes the fork's lock once it has done
    /// its waiting on the network and reads again ([`Store::reread_locked`]).
    ///
    /// `former` is what the caller's registry says each fork was once kept
    /// under ([`crate::config::Registry::former_names`]), for the refusal of
    /// a state file an older knives left ([`Store::migrated`]). `repos` are
    /// the repositories whose branch statements the caller will ask about,
    /// and only their ledgers are read: a ledger costs a read of its whole
    /// history, which a command about one fork must not pay for every other.
    /// Either open fails on a ledger it cannot read, as it does on a state file
    /// it cannot read: a statement the store could not read must not answer as
    /// no statement.
    pub fn open(
        path: PathBuf,
        former: &FormerNames,
        repos: &[&UpstreamName],
    ) -> Result<Self, StoreError> {
        Self::read(path, repos, Writes::Replayed(Vec::new()), Vec::new())?
            .migrated(former)?
            .all_read()
    }

    /// For a decision about `repos` and the write that records it: a claim
    /// taken or released, a statement replacing the one before it, an event
    /// for a pull request state that changed since the last one. Takes the
    /// lock of each fork in `repos` before reading anything and holds it until
    /// dropped, so another writer about the same fork waits for this one and
    /// then reads what it wrote; it waits the full claim-writer budget
    /// ([`LockWait::CLAIM`]) for one. A fork's lock keeps out no writer about
    /// any other fork: the state file's own lock is taken only by
    /// [`Store::save`], for the one write. `former` and `repos` as for
    /// [`Store::open`].
    pub fn open_for_update(
        path: PathBuf,
        former: &FormerNames,
        repos: &[&UpstreamName],
    ) -> Result<Self, StoreError> {
        let forks = lock_forks(&path, repos)?;
        Self::read(path, repos, Writes::Replayed(Vec::new()), forks)?
            .migrated(former)?
            .all_read()
    }

    /// [`Store::open`] for a report of many forks, each its own row: a
    /// ledger it cannot read is kept for that fork ([`Store::unreadable`])
    /// instead of failing the open, so one fork's broken ledger blanks no
    /// other fork's row. Nothing about such a fork may be asked of the store;
    /// its row is that it could not be read.
    pub fn open_each(
        path: PathBuf,
        former: &FormerNames,
        repos: &[&UpstreamName],
    ) -> Result<Self, StoreError> {
        Self::read(path, repos, Writes::Replayed(Vec::new()), Vec::new())?.migrated(former)
    }

    /// Take the lock of each fork this store was opened for, unless it holds
    /// them already, and read the state file and those forks' ledgers again
    /// under it; the locks are held until the returned value is dropped.
    ///
    /// For a writer that asks the network before it decides, and must not
    /// keep every other writer of the fork waiting on that: it opens the
    /// store with no lock ([`Store::open`], [`Store::open_each`]), asks, and
    /// then takes the lock here for its decision and its write. What the
    /// store answers from then on is what the fork's other writers left, so
    /// the writer can check that what it read before its wait has not moved.
    /// A change made on the store before is kept: it applies to what is read
    /// here, as the save replays it.
    ///
    /// The refusal of a state file an older knives left is not made again:
    /// the open made it ([`Store::migrated`]), and `knives ledger migrate`,
    /// the only writer that rewrites one, holds the state file's lock for its
    /// whole run, which [`Store::save`] waits for as well.
    ///
    /// # Errors
    ///
    /// [`StoreError::Lock`] when another writer of one of the forks still
    /// holds its lock after the wait ([`LockWait::CLAIM`]); a state file that
    /// cannot be read; and [`StoreError::Ledger`] with why the first of the
    /// ledgers that can no longer be read could not be.
    pub fn reread_locked(&mut self) -> Result<ForkLock, StoreError> {
        let changes = match &self.writes {
            Writes::Replayed(changes) => changes,
            // `knives ledger migrate` writes the whole file, holding the
            // state file's lock from its open while its caller holds every
            // fork's lock, so nothing it read can have moved.
            Writes::Whole(_) => return Ok(ForkLock { _held: Vec::new() }),
        };
        let owned: Vec<UpstreamName> = self.ledgers.keys().cloned().collect();
        let repos: Vec<&UpstreamName> = owned.iter().collect();
        let held = if self.forks.is_empty() {
            lock_forks(&self.path, &repos)?
        } else {
            Vec::new()
        };
        let mut state = read_state(&self.path)?;
        for change in changes {
            // As at [`Store::change`]: only a save can find the file moved
            // under a change, and the save replays it then.
            let _ = change(&mut state, &self.path);
        }
        self.state = state;
        self.ledgers = read_ledgers(&self.path.with_file_name("ledger"), &repos);
        self.take_unreadable()?;
        Ok(ForkLock { _held: held })
    }

    /// Refuse a store that could not read one of the ledgers it was opened
    /// for, naming the first: what [`Store::open`] and
    /// [`Store::open_for_update`] do on opening, for a store opened with
    /// [`Store::open_each`] once it needs every ledger.
    ///
    /// # Errors
    ///
    /// [`StoreError::Ledger`] with why the first unreadable ledger could not
    /// be read.
    pub fn all_read(mut self) -> Result<Self, StoreError> {
        self.take_unreadable()?;
        Ok(self)
    }

    /// [`Store::all_read`] in place: the first ledger the store could not
    /// read leaves it, as its error.
    fn take_unreadable(&mut self) -> Result<(), StoreError> {
        let unreadable = self
            .ledgers
            .iter()
            .find_map(|(repo, read)| read.is_err().then(|| repo.clone()));
        match unreadable.and_then(|repo| self.ledgers.remove(&repo)) {
            Some(Err(error)) => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Refuse a state file an older knives left: one still holding the maps
    /// it kept branch statements in, or still keeping a fork under the
    /// registry key it was known by before each fork was named after its
    /// upstream repository. This knives reads statements only from the
    /// ledger, and looks a fork up only by its upstream name, so such a file
    /// would report stated pull requests, fork-only branches, supersessions,
    /// dependencies and claims as absent, which reads exactly like a clean
    /// slate. A refusal naming the command that fixes it is the only honest
    /// answer.
    ///
    /// Which keys are former names is the registry's to say: `former` is
    /// what the caller's registry says ([`crate::config::Registry::former_names`]),
    /// the one it read once for the whole command. The ledger is read from
    /// beside the state file, so a store opened anywhere else answers for
    /// what sits there. A former name's ledger directory is unmigrated while
    /// it holds an entry file; one holding only other forks' directories is
    /// the owner directory of their upstream names (`acme` beside
    /// `acme/demo`).
    fn migrated(self, former: &FormerNames) -> Result<Self, StoreError> {
        let held: Vec<&str> = LEGACY_STATEMENTS
            .into_iter()
            .filter(|name| self.state.extra.contains_key(*name))
            .collect();
        if !held.is_empty() {
            return Err(StoreError::Unmigrated {
                path: self.path,
                maps: held.join(", "),
            });
        }
        let root = self.path.with_file_name("ledger");
        let maps: BTreeSet<StateMap> = self
            .state
            .renames(former)
            .into_iter()
            .map(|renamed| renamed.map)
            .collect();
        let held: Vec<String> = maps
            .iter()
            .map(ToString::to_string)
            .chain(unmigrated(&root, former).map(|(name, _)| root.join(name).display().to_string()))
            .collect();
        if held.is_empty() {
            Ok(self)
        } else {
            Err(StoreError::FormerNames {
                path: self.path,
                held: held.join(", "),
            })
        }
    }

    fn read(
        path: PathBuf,
        repos: &[&UpstreamName],
        writes: Writes,
        forks: Vec<FileLock>,
    ) -> Result<Self, StoreError> {
        let state = read_state(&path)?;
        let ledgers = read_ledgers(&path.with_file_name("ledger"), repos);
        Ok(Self {
            path,
            state,
            ledgers,
            writes,
            forks,
        })
    }

    /// Record `change`: apply it to what this store answers from now, and
    /// keep it for the save to replay.
    fn change(&mut self, change: Change) {
        // Applied to what this store itself read and changed, a change finds
        // what it expects: only a save, onto the file as it is by then, can
        // find the file moved.
        let _ = change(&mut self.state, &self.path);
        if let Writes::Replayed(changes) = &mut self.writes {
            changes.push(change);
        }
    }

    /// Write what this store changed. Each change is replayed onto the
    /// state file as it is now, under the state file's lock held only for
    /// this read and write, so a change another writer saved since this
    /// store opened survives; a store that changed nothing writes nothing.
    /// A claim another writer moved since this store read it fails the
    /// whole save ([`StoreError::ClaimMoved`]), and nothing is written.
    ///
    /// The write is atomic, so a crash mid-write cannot truncate the file and
    /// a concurrent reader never sees a half-written document.
    pub fn save(&mut self) -> Result<(), StoreError> {
        let changes = match &mut self.writes {
            Writes::Whole(_) => return write_state(&self.path, &self.state),
            Writes::Replayed(changes) if changes.is_empty() => return Ok(()),
            Writes::Replayed(changes) => changes,
        };
        let _lock = FileLock::acquire(&self.path, LockWait::CLAIM)?;
        let mut state = read_state(&self.path)?;
        for change in changes.iter() {
            change(&mut state, &self.path)?;
        }
        write_state(&self.path, &state)?;
        changes.clear();
        self.state = state;
        Ok(())
    }

    /// Take a local claim of `target` for `identity`: one this machine keeps
    /// to itself, as on a fork that shares no ledger.
    pub fn claim(&mut self, target: &BranchTarget, identity: &Identity, why: &str) -> Claim {
        let record = Claim {
            repo: target.repo.to_string(),
            branch: target.branch.to_string(),
            owner: identity.owner.clone(),
            kind: identity.kind,
            why: why.to_owned(),
            started: jiff::Timestamp::now().to_string(),
            files: Vec::new(),
            place: Some(crate::liveness::this_place()),
            machine: None,
            ledger_commit: None,
        };
        self.set_claim(record.key(), Some(record.clone()));
        record
    }

    /// Put `copy`, this machine's copy of the ledger's claim of its branch
    /// ([`Claim::ledger_commit`]), in place of whatever this store holds for
    /// that branch: a local claim, an older copy, or nothing.
    ///
    /// The ledger's claim ref decides who holds the branch, so what this
    /// file held for it is replaced whole, its `started` and `place`
    /// included: they are the ledger claim's, wherever it was taken.
    pub fn copy_ledger_claim(&mut self, copy: Claim) {
        self.set_claim(copy.key(), Some(copy));
    }

    /// Put `why` in place of the reason the claim on `target` carries, and
    /// change nothing else about it; `None` where nothing is claimed there.
    ///
    /// The reason a reader is shown must be the reason the work is held for
    /// now, not the one its first `start` gave days ago. Only the reason
    /// moves: [`Store::claim`] sets a new `started` and a new `place`,
    /// which are when and where the claim was taken and belong to that
    /// moment alone, so taking work up again must not write them.
    pub fn restate_claim(&mut self, target: &BranchTarget, why: &str) -> Option<Claim> {
        let key = target.key();
        let mut record = self.state.claims.get(&key)?.clone();
        why.clone_into(&mut record.why);
        self.set_claim(key, Some(record.clone()));
        Some(record)
    }

    pub fn release_claim(&mut self, target: &BranchTarget) -> bool {
        let key = target.key();
        let had = self.state.claims.contains_key(&key);
        if had {
            self.set_claim(key, None);
        }
        had
    }

    /// Put `next` in place of the claim at `key` that this store holds now.
    /// The decision to take or release a claim was made from that one, so
    /// the save refuses when the file holds another: a writer that bypassed
    /// the branch's lock (an older knives, a hand edit) moved it meanwhile.
    ///
    /// The owner that moved it is that writer's own text, and the refusal is
    /// printed to a terminal, so it goes out escaped
    /// ([`inline_human_text`]) while the state file keeps the raw name.
    fn set_claim(&mut self, key: String, next: Option<Claim>) {
        let read = self.state.claims.get(&key).cloned();
        self.change(Box::new(move |state, path| {
            let now = state.claims.get(&key);
            if now != read.as_ref() {
                return Err(StoreError::ClaimMoved {
                    path: path.to_owned(),
                    key: key.clone(),
                    now: now.map_or_else(
                        || "now unheld".to_owned(),
                        |claim| format!("now held by {}", inline_human_text(&claim.owner)),
                    ),
                });
            }
            match &next {
                Some(claim) => {
                    let _ = state.claims.insert(key.clone(), claim.clone());
                }
                None => {
                    let _ = state.claims.remove(&key);
                }
            }
            Ok(())
        }));
    }

    pub fn claims(&self, repo: Option<&UpstreamName>) -> Vec<&Claim> {
        self.state
            .claims
            .values()
            .filter(|claim| repo.is_none_or(|name| claim.repo == name.as_str()))
            .collect()
    }

    /// What this store holds for the branch `target`, if anything: a claim
    /// this machine took, or its copy of a ledger's claim.
    pub fn claim_of(&self, target: &BranchTarget) -> Option<&Claim> {
        self.state.claims.values().find(|claim| {
            claim.repo == target.repo.as_str() && claim.branch == target.branch.as_str()
        })
    }

    pub fn current_agent(&self) -> Option<&str> {
        self.state
            .extra
            .get("currentAgent")
            .or_else(|| self.state.extra.get("current_agent"))
            .and_then(serde_json::Value::as_str)
            .filter(|agent| !agent.trim().is_empty())
    }

    /// Without this statement, a branch we deliberately keep with no upstream
    /// pull request reads as an error in every status report, forever.
    ///
    /// This and the other statement accessors panic for a repository the store
    /// was not opened for: that is a knives bug, and answering "no statement"
    /// would hide it behind a wrong report.
    pub fn is_fork_only(&self, target: &BranchTarget) -> bool {
        self.stated_in(&target.repo)
            .fork_only(target.branch.as_str())
    }

    pub fn record_foreign_parent(&mut self, repo: &UpstreamName, number: u64, why: &str) {
        let (key, why) = (format!("{repo}/{number}"), why.to_owned());
        self.change(Box::new(move |state, _| {
            let _ = state.foreign_parents.insert(key.clone(), why.clone());
            Ok(())
        }));
    }

    /// Pull request numbers we carry as release parents but did not author.
    ///
    /// A release parent can be any upstream pull request, including a
    /// maintainer's, so these are tracked even though no branch of ours matches.
    pub fn foreign_parent_numbers(&self, repo: &UpstreamName) -> Vec<u64> {
        let prefix = format!("{repo}/");
        self.state
            .foreign_parents
            .keys()
            .filter_map(|key| key.strip_prefix(&prefix))
            .filter_map(|tail| tail.trim_start_matches('#').parse().ok())
            .collect()
    }

    /// Where one branch's work continued, if it was stated.
    ///
    /// Distinguishing supersession from a staleness bot closing a live branch,
    /// and from a deliberate fork-only branch, needs intent. Intent cannot be
    /// recomputed, so `finish --superseded-by` states it.
    pub fn superseded_by(&self, target: &BranchTarget) -> Option<&str> {
        self.stated_in(&target.repo)
            .superseded_by(target.branch.as_str())
    }

    /// What `target` cannot land before.
    pub fn dependencies(&self, target: &BranchTarget) -> Vec<Requirement> {
        self.stated_in(&target.repo)
            .depends(target.branch.as_str())
            .iter()
            .filter_map(|text| Requirement::parse(text))
            .collect()
    }

    /// Every branch of `repo` that requires something now, with the newest
    /// commit the ledger records for it, where any entry records one
    /// ([`crate::statement::Statements::recorded_head`]).
    pub fn dependents(&self, repo: &UpstreamName) -> Vec<(BranchName, Option<CommitId>)> {
        let stated = self.stated_in(repo);
        stated
            .dependents()
            .map(|branch| {
                (
                    BranchName::new(branch),
                    stated.recorded_head(branch).map(CommitId::new),
                )
            })
            .collect()
    }

    /// The pull request stated for `target`, if any. Overrides inference.
    ///
    /// Inference matches an open pull request from our own copy of the
    /// repository, which is right as a default and wrong as the only option. A
    /// pull request opened before this tool existed cannot be found that way;
    /// neither can one that was closed because the maintainer wanted something
    /// else, nor somebody else's that we are carrying because ours was
    /// superseded. A statement accepts any number in any state from any author.
    pub fn tracked_pull(&self, target: &BranchTarget) -> Option<u64> {
        self.stated_in(&target.repo).pull(target.branch.as_str())
    }

    /// The stamp of the live statement of `kind` about `target`, which a
    /// statement replacing it is stamped after ([`crate::ledger::Stated::replaces`]).
    /// It is the newest this machine has while the reader holds the lock
    /// every writer of that statement on this machine takes: the fork's
    /// ([`Store::open_for_update`]) for `track` and `depends`, and for
    /// `superseded`, which only `finish` writes, the branch's ([`BranchLock`]).
    pub fn statement_stamp(
        &self,
        target: &BranchTarget,
        kind: StatementKind,
    ) -> Option<jiff::Timestamp> {
        self.stated_in(&target.repo)
            .stamp(target.branch.as_str(), kind)
    }

    /// What reading `repo`'s ledger found it could read only in part: an
    /// entry of a kind this knives does not know, or a statement it cannot
    /// read ([`Ledger::read`]). A report about `repo` carries each as a
    /// problem, since what the statements answer may lack what that entry
    /// states.
    ///
    /// Unlike the other ledger accessors, this answers for any repository:
    /// nothing for one the store was not opened for, or whose ledger it could
    /// not read ([`Store::unreadable`]). A report's row for an entry the scan
    /// did not place, which is never opened, asks it too.
    pub fn ledger_problems(&self, repo: &UpstreamName) -> &[String] {
        match self.ledgers.get(repo) {
            Some(Ok(ledger)) => &ledger.problems,
            _ => &[],
        }
    }
    /// Why `repo`'s ledger could not be read at all, for a store opened with
    /// [`Store::open_each`]: that fork's answer is this, and nothing else
    /// about its ledger may be asked of the store.
    pub fn unreadable(&self, repo: &UpstreamName) -> Option<&LedgerError> {
        self.ledgers.get(repo)?.as_ref().err()
    }

    /// Every entry of `repo`'s ledger this knives can read, oldest first, as
    /// the store read it when it opened ([`Ledger::read`]).
    ///
    /// # Panics
    ///
    /// When the store was not opened for `repo`, as asking it about a
    /// statement there does.
    pub fn ledger_entries(&self, repo: &UpstreamName) -> &[Entry] {
        &self.fork(repo, "ledger entries").entries
    }

    fn stated_in(&self, repo: &UpstreamName) -> &Statements {
        &self.fork(repo, "branch statements").statements
    }

    /// What the store read of `repo`'s ledger, for asking it about `what`.
    #[allow(
        clippy::panic,
        reason = "a store asked about a repository it was not opened for is a knives bug; answering no statement or no entries would be the silent wrong report this exists to prevent"
    )]
    fn fork(&self, repo: &UpstreamName, what: &str) -> &ForkLedger {
        match self.ledgers.get(repo) {
            Some(Ok(ledger)) => ledger,
            _ => panic!(
                "knives bug: asked about {repo}'s {what}, but the store was opened without {repo}'s ledger"
            ),
        }
    }

    pub fn convention_digest(&self, repo: &UpstreamName, file: &str) -> Option<&str> {
        self.state
            .conventions
            .get(&format!("{repo}/{file}"))
            .map(String::as_str)
    }

    pub fn record_convention_digest(&mut self, repo: &UpstreamName, file: &str, digest: &str) {
        let (key, digest) = (format!("{repo}/{file}"), digest.to_owned());
        self.change(Box::new(move |state, _| {
            let _ = state.conventions.insert(key.clone(), digest.clone());
            Ok(())
        }));
    }

    pub fn pull_heads(&self, repo: &UpstreamName) -> BTreeMap<String, String> {
        self.state
            .pull_heads
            .get(repo.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn record_pull_head(&mut self, repo: &UpstreamName, number: u64, sha: &str) {
        let (repo, number, sha) = (repo.to_string(), number.to_string(), sha.to_owned());
        self.change(Box::new(move |state, _| {
            let _ = state
                .pull_heads
                .entry(repo.clone())
                .or_default()
                .insert(number.clone(), sha.clone());
            Ok(())
        }));
    }

    /// Record where the forge said a pull request stands. The parameter type is
    /// the invariant: a record is a forge state, never the transition sync
    /// classified from it.
    pub fn record_pull_state(&mut self, repo: &UpstreamName, number: u64, state: ForgeState) {
        let (key, recorded) = (format!("{repo}#{number}"), state.as_recorded().to_owned());
        self.change(Box::new(move |state, _| {
            let _ = state.pull_states.insert(key.clone(), recorded.clone());
            Ok(())
        }));
    }

    /// Drop a record this version cannot read, so its problem is reported once.
    pub fn forget_pull_state(&mut self, repo: &UpstreamName, number: u64) {
        let key = format!("{repo}#{number}");
        self.change(Box::new(move |state, _| {
            let _ = state.pull_states.remove(&key);
            Ok(())
        }));
    }

    /// The recorded state as the file spells it: `record_pull_state` writes a
    /// forge state, but a file an earlier version wrote may spell otherwise, and
    /// the reader decides what that means.
    pub fn pull_state(&self, repo: &UpstreamName, number: u64) -> Option<&str> {
        self.state
            .pull_states
            .get(&format!("{repo}#{number}"))
            .map(String::as_str)
    }

    pub fn record_comment_mark(&mut self, repo: &UpstreamName, number: u64, at: &str) {
        let (key, at) = (format!("{repo}#{number}"), at.to_owned());
        self.change(Box::new(move |state, _| {
            let _ = state.comment_marks.insert(key.clone(), at.clone());
            Ok(())
        }));
    }

    pub fn comment_mark(&self, repo: &UpstreamName, number: u64) -> Option<&str> {
        self.state
            .comment_marks
            .get(&format!("{repo}#{number}"))
            .map(String::as_str)
    }
}

/// The lock of each fork in `repos`, taken in name order so that two writers
/// about the same forks cannot each hold one the other waits for. A fork's
/// lock is `fork-locks/<upstream>.lock` beside the state file: it guards the
/// fork's statements of `track` and `depends`, the pull request records
/// `sync` keeps in the state file, and the release verbs' ledger events
/// ([`ForkLock`]), and nothing else. A claim is guarded by its branch's lock
/// ([`BranchLock`]).
pub(crate) fn lock_forks(path: &Path, repos: &[&UpstreamName]) -> Result<Vec<FileLock>, LockError> {
    let ordered: BTreeSet<&UpstreamName> = repos.iter().copied().collect();
    ordered
        .into_iter()
        .map(|repo| {
            // Named `<upstream>.fork` for `FileLock` to put `.lock` in place
            // of: an upstream name's own dots are not an extension.
            let target = path
                .with_file_name("fork-locks")
                .join(format!("{}.fork", repo.as_str()));
            FileLock::acquire(&target, LockWait::CLAIM)
        })
        .collect()
}

/// One fork's lock ([`lock_forks`]), held by a writer that opens no store
/// for update.
///
/// A release verb holds it from the first read it decides from, once it has
/// asked the network, until its last write; `finish`'s sweep of a release's
/// earlier writes holds it for that sweep alone.
///
/// The lock is the kernel's lock on one open file, so a second acquire in
/// the same process waits for the first one like any other waiter, and then
/// gives up. So a release verb takes it once, and nothing it runs under it
/// takes it again: it opens the store only to read ([`Store::open`], which
/// takes no lock), and an append to the ledger takes none
/// ([`Ledger::append`]).
#[derive(Debug)]
pub struct ForkLock {
    /// Dropping it releases the lock.
    _held: Vec<FileLock>,
}

impl ForkLock {
    /// The lock of `repo` beside the state file at its default path, waited
    /// for as a claim writer waits ([`LockWait::CLAIM`]).
    ///
    /// # Errors
    ///
    /// [`LockError::Held`] when another command still holds it after the
    /// wait, naming the holder's pid where the lock file holds one, else
    /// how long ago the lock was written; [`LockError::Io`] when its file
    /// cannot be opened.
    pub fn acquire(repo: &UpstreamName) -> Result<Self, LockError> {
        Self::acquire_beside(&default_state_path(), repo)
    }

    /// The lock of `repo` beside the state file at `state`
    /// ([`ForkLock::acquire`]).
    ///
    /// # Errors
    ///
    /// As [`ForkLock::acquire`].
    pub fn acquire_beside(state: &Path, repo: &UpstreamName) -> Result<Self, LockError> {
        Ok(Self {
            _held: lock_forks(state, &[repo])?,
        })
    }
}

/// `repo` as one file name: each `%` as `%25`, each `/` as `%2F` and a
/// leading `.` as `%2E`, or its digest where that is too long ([`fitted`]).
///
/// So two upstreams never name one lock file, and no lock of one upstream is
/// a directory of another's. The sync and branch locks each live under a
/// root of their own beside the state file, apart from the fork locks, so
/// none of them is a fork's lock either.
fn lock_component(repo: &UpstreamName) -> String {
    let mut encoded = repo.as_str().replace('%', "%25").replace('/', "%2F");
    if encoded.starts_with('.') {
        encoded.replace_range(..1, "%2E");
    }
    fitted(encoded)
}

/// `name`, which starts with no `.`, where it leaves room for `.lock` in a
/// file name, which is at most 255 bytes where knives runs; else
/// `.sha256-<its SHA-256 in hex>`, which no such name is.
fn fitted(name: String) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    const NAME_MAX: usize = 255;
    if name.len() + ".lock".len() <= NAME_MAX {
        return name;
    }
    let digest: [u8; 32] = sha2::Sha256::digest(name.as_bytes()).into();
    let mut fitted = String::with_capacity(".sha256-".len() + 64);
    fitted.push_str(".sha256-");
    for byte in digest {
        let _ = write!(fitted, "{byte:02x}");
    }
    fitted
}

/// One fork's sync lock, held by `sync` of that fork for its whole run.
///
/// It covers the network too, so two syncs of one fork take turns: `sync` is
/// the only writer of the pull request records, so no record can change
/// while a sync asks the forge, and none can change and change back. Nothing
/// else takes it, so no other writer of the fork waits on a sync's network;
/// `sync` takes it before the fork's lock, which it holds for its writes
/// alone ([`Store::reread_locked`]).
///
/// The file is `sync-locks/<upstream>.lock` beside the state file, its
/// upstream one file name ([`lock_component`]).
#[derive(Debug)]
pub struct SyncLock {
    /// Dropping it releases the lock.
    _held: FileLock,
}

impl SyncLock {
    /// The sync lock of `repo` beside the state file at its default path
    /// ([`SyncLock::acquire_beside`]).
    ///
    /// # Errors
    ///
    /// As [`ForkLock::acquire`].
    pub fn acquire(repo: &UpstreamName) -> Result<Self, LockError> {
        Self::acquire_beside(&default_state_path(), repo)
    }

    /// The sync lock of `repo` beside the state file at `state`, waited for
    /// as a claim writer waits ([`LockWait::CLAIM`]).
    ///
    /// # Errors
    ///
    /// As [`ForkLock::acquire`].
    pub fn acquire_beside(state: &Path, repo: &UpstreamName) -> Result<Self, LockError> {
        // Named `<upstream>.sync` for `FileLock` to put `.lock` in place of:
        // an upstream name's own dots are not an extension.
        let target = state
            .with_file_name("sync-locks")
            .join(format!("{}.sync", lock_component(repo)));
        Ok(Self {
            _held: FileLock::acquire(&target, LockWait::CLAIM)?,
        })
    }
}

/// One branch's lock, held by `start` and `finish` of that branch.
///
/// They hold it from before their first read until their last write: a
/// `start` and a `finish` of one branch take turns, and the later one
/// decides from what the earlier one wrote, while those of two branches of
/// one fork run side by side.
///
/// Neither holds its fork's lock ([`lock_forks`]) across its run: both wait
/// on the network (the ledger's claim ref and its push, `jj git fetch`) and
/// on slow local work (a snapshot, a workspace's removal), and a fork's lock
/// held across that keeps every other writer of the fork waiting as long.
/// What the fork's lock kept for them is kept another way:
///
/// - a branch's claim is written only by a `start` or a `finish` of that
///   branch, which take this lock, and the save compares the claim it writes
///   over with the one the command read ([`StoreError::ClaimMoved`]);
/// - `superseded`, the one statement `finish` writes, only `knives ledger
///   migrate` also writes, and a store refuses to open while the state file
///   holds what that migrates ([`StoreError::Unmigrated`]); `start` writes
///   no statement;
/// - the ledger's claim ref changes only by a push leased on the claim
///   commit the command read; that push also moves this machine's own ref,
///   leased on its last commit, under the ledger's lock and its git
///   directory's transport lock, so the claim pushes of one machine still
///   take turns there;
/// - `finish`'s sweep of a release's earlier writes, which a release write
///   must not meet, takes the fork's lock for the sweep alone ([`ForkLock`]);
/// - a reap, which keeps a release a claim holds, tries the lock of each
///   release name under the fork's lock, and keeps the release whose lock a
///   `start` or `finish` holds ([`BranchLock::try_acquire`]).
///
/// It is taken before any other lock the command takes: the fork's, the
/// ledger's and the transport lock, in that order, as every other command
/// takes those it takes, so no two commands each hold one the other waits
/// for; the reap's tries under the fork's lock wait for nothing.
///
/// The lock is the workspace's the branch's work lives in, named with the
/// branch's slashes flattened ([`crate::commands::wip::workspace_for`]), so
/// two branch names that share one workspace, `feat/a` and `feat-a`, share
/// one lock too: `branch-locks/<upstream>/<workspace>.lock` beside the state
/// file, its upstream one directory name ([`lock_component`]), one file each.
#[derive(Debug)]
pub struct BranchLock {
    /// Dropping it releases the lock.
    _held: FileLock,
}

impl BranchLock {
    /// The lock of `branch` of `repo` beside the state file at its default
    /// path ([`BranchLock::acquire_beside`]).
    ///
    /// # Errors
    ///
    /// As [`ForkLock::acquire`].
    pub fn acquire(repo: &UpstreamName, branch: &BranchName) -> Result<Self, LockError> {
        Self::acquire_beside(&default_state_path(), repo, branch)
    }

    /// The lock of `branch` of `repo` beside the state file at `state`,
    /// waited for as a claim writer waits ([`LockWait::CLAIM`]).
    ///
    /// # Errors
    ///
    /// As [`ForkLock::acquire`].
    pub fn acquire_beside(
        state: &Path,
        repo: &UpstreamName,
        branch: &BranchName,
    ) -> Result<Self, LockError> {
        Self::acquire_at(state, repo, branch, LockWait::CLAIM)
    }

    /// The lock of `branch` of `repo` beside the state file at its default
    /// path, without waiting: `None` while a `start` or `finish` of the
    /// branch holds it.
    ///
    /// # Errors
    ///
    /// [`LockError::Io`] when its file cannot be opened.
    pub fn try_acquire(
        repo: &UpstreamName,
        branch: &BranchName,
    ) -> Result<Option<Self>, LockError> {
        Ok(
            FileLock::try_acquire(&Self::target(&default_state_path(), repo, branch))?
                .map(|held| Self { _held: held }),
        )
    }

    fn acquire_at(
        state: &Path,
        repo: &UpstreamName,
        branch: &BranchName,
        wait: LockWait,
    ) -> Result<Self, LockError> {
        Ok(Self {
            _held: FileLock::acquire(&Self::target(state, repo, branch), wait)?,
        })
    }

    /// What [`FileLock`] locks for `branch` of `repo` beside the state file
    /// at `state`.
    fn target(state: &Path, repo: &UpstreamName, branch: &BranchName) -> PathBuf {
        // A workspace name starts with no `.` ([`BranchName::parse`]), so its
        // digest, where it is too long for a file name, is no workspace's.
        let stem = fitted(crate::commands::wip::workspace_for(branch.as_str()));
        // Named `<stem>.branch` for `FileLock` to put `.lock` in place of: a
        // workspace name's own dots are not an extension, so `fix-v1.2` and
        // `fix-v1` are two locks.
        state
            .with_file_name("branch-locks")
            .join(lock_component(repo))
            .join(format!("{stem}.branch"))
    }
}

/// The state file at `path`, or the empty state when there is none yet.
fn read_state(path: &Path) -> Result<State, StoreError> {
    if !path.exists() {
        return Ok(State::default());
    }
    let text = std::fs::read_to_string(path).map_err(|source| StoreError::Read {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| StoreError::Parse {
        path: path.to_owned(),
        source,
    })
}

/// Write `state` to `path` atomically: a crash mid-write cannot truncate the
/// file, and a concurrent reader never sees a half-written document.
fn write_state(path: &Path, state: &State) -> Result<(), StoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
        path: parent.to_owned(),
        source,
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|source| StoreError::Write {
        path: parent.to_owned(),
        source,
    })?;
    let text = serde_json::to_string_pretty(state)?;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.write_all(b"\n"))
        .map_err(|source| StoreError::Write {
            path: path.to_owned(),
            source,
        })?;
    temp.persist(path).map_err(|error| StoreError::Write {
        path: path.to_owned(),
        source: error.error,
    })?;
    Ok(())
}

/// Each of `repos`' ledgers beside the state file, as one read found it
/// ([`ForkLedger`]), or why it could not be read at all.
///
/// `ledger/<owner>/<name>/` beside `state.json` is where
/// [`crate::ledger::default_ledger_path`] puts each fork's ledger, so a store
/// opened at the default path reads the default ledgers and one opened
/// anywhere else reads only what sits beside it. A repository nobody has
/// stated anything about yet has no ledger, and so no statements.
fn read_ledgers(
    root: &Path,
    repos: &[&UpstreamName],
) -> BTreeMap<UpstreamName, Result<ForkLedger, LedgerError>> {
    repos
        .iter()
        .map(|repo| {
            let read = Ledger::at(root.join(repo.as_str()))
                .read()
                .map(|read| ForkLedger {
                    statements: Statements::from_entries(&read.entries),
                    entries: read.entries,
                    problems: read.problems,
                });
            ((*repo).clone(), read)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ids::BranchName;
    use crate::ledger::{Entry, Kind};
    use crate::statement::{Statement, StatementKind};

    /// A store over `dir`'s state file, open for `a-repo`'s statements.
    fn store(dir: &Path) -> Store {
        Store::open(
            dir.join("state.json"),
            &crate::config::FormerNames::new(),
            &[&repo()],
        )
        .unwrap()
    }

    /// Append to `a-repo`'s ledger beside `dir`'s state file an event stating
    /// `kind` about `branch`, as `track`, `depends` and `finish` write one.
    #[allow(
        clippy::too_many_arguments,
        reason = "a fixture: where, when, about which branch, and the statement are independent"
    )]
    fn state(dir: &Path, ts: &str, branch: &str, kind: StatementKind, value: Option<&str>) {
        Ledger::at(dir.join("ledger").join("a-repo"))
            .append(&Entry {
                ts: ts.to_owned(),
                owner: "ses_fff688".to_owned(),
                email: None,
                subject: Some(branch.to_owned()),
                kind: Kind::Event,
                disposition: None,
                statement: Some(Statement {
                    kind,
                    value: value.map(str::to_owned),
                }),
                text: "stated".to_owned(),
                evidence: Vec::new(),
                anchor: None,
                anchor_ref: None,
                pr: None,
                parents: Vec::new(),
                fix_files: Vec::new(),
                claim_change: None,
                claim_change_why: None,
                previous_owner: None,
                previous_kind: None,
                previous_started: None,
                previous_last_seen: None,
                previous_why: None,
            })
            .unwrap();
    }

    fn repo() -> UpstreamName {
        UpstreamName::new("a-repo")
    }

    fn target() -> BranchTarget {
        BranchTarget::new(
            UpstreamName::new("a-repo"),
            crate::ids::BranchName::new("feat/alpha"),
        )
    }

    fn os_user(owner: &str) -> Identity {
        Identity {
            owner: owner.to_owned(),
            kind: OwnerKind::OsUser,
        }
    }

    #[test]
    fn a_harness_claim_writes_and_round_trips_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let identity = crate::commands::claim::Identity {
            owner: "someone".to_owned(),
            kind: OwnerKind::HarnessSession,
        };
        let mut first = Store::open(path.clone(), &crate::config::FormerNames::new(), &[]).unwrap();
        let _ = first.claim(&target(), &identity, "fixing the parser");
        first.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#""kind": "harness-session""#),
            "state was: {text}"
        );

        let reloaded = Store::open(path, &crate::config::FormerNames::new(), &[]).unwrap();
        let claims = reloaded.claims(None);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].why, "fixing the parser");
        assert_eq!(claims[0].kind, OwnerKind::HarnessSession);
        assert_ne!(claims[0].started, "");
    }

    #[test]
    fn a_legacy_claim_without_kind_defaults_to_an_os_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"claims":{"a-repo/feat/alpha":{"repo":"a-repo","branch":"feat/alpha","owner":"someone","why":"legacy claim","started":"2026-01-01T00:00:00Z","files":[]}}}"#,
        )
        .unwrap();

        let store = Store::open(path, &crate::config::FormerNames::new(), &[]).unwrap();
        let claims = store.claims(None);

        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].kind, OwnerKind::OsUser);
    }

    #[test]
    fn releasing_a_claim_reports_whether_there_was_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        let _ = subject.claim(&target(), &os_user("someone"), "w");
        assert!(subject.release_claim(&target()));
        assert!(!subject.release_claim(&target()));
        assert_eq!(subject.claims(None), Vec::<&Claim>::new());
    }

    #[test]
    fn restating_a_claim_writes_its_reason_and_leaves_when_and_where_it_was_taken() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        let taken = subject.claim(&target(), &os_user("someone"), "port it");

        let restated = subject.restate_claim(&target(), "finish the port").unwrap();

        assert_eq!(restated.why, "finish the port");
        assert_eq!(restated.started, taken.started);
        assert_eq!(restated.place, taken.place);
        assert_eq!(subject.claims(None), vec![&restated]);
    }

    #[test]
    fn restating_a_reason_where_nothing_is_claimed_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());

        assert_eq!(subject.restate_claim(&target(), "port it"), None);
        assert_eq!(subject.claims(None), Vec::<&Claim>::new());
    }

    #[test]
    fn a_local_claim_writes_no_machine_and_no_ledger_commit() {
        // Given: a claim taken on a fork that shares no ledger.
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        let _ = subject.claim(&target(), &os_user("someone"), "port it");

        // When: it is saved.
        subject.save().unwrap();

        // Then: the state file holds neither key, as a knives before them
        // wrote it.
        let text = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
        assert!(!text.contains("\"machine\""), "state was: {text}");
        assert!(!text.contains("\"ledger_commit\""), "state was: {text}");
    }

    #[test]
    fn a_copy_of_a_ledger_claim_replaces_the_claim_held_and_round_trips() {
        // Given: a local claim of the branch.
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        let local = subject.claim(&target(), &os_user("someone"), "port it");
        subject.save().unwrap();
        let mut copying = store(dir.path());

        // When: a copy of the ledger's claim of it is put in its place.
        let copy = Claim {
            owner: "owner-two".to_owned(),
            kind: OwnerKind::HarnessSession,
            started: "2026-11-16T08:00:00Z".to_owned(),
            place: Some("box-two 4026531836".to_owned()),
            machine: Some("machine-two".to_owned()),
            ledger_commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            ..local
        };
        copying.copy_ledger_claim(copy.clone());
        copying.save().unwrap();

        // Then: the state file holds the copy whole, its machine and its
        // claim commit with it.
        let reloaded = store(dir.path());
        assert_eq!(reloaded.claims(None), vec![&copy]);
    }

    #[test]
    fn claims_can_be_filtered_by_repo() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        let _ = subject.claim(
            &BranchTarget::new(
                UpstreamName::new("one"),
                crate::ids::BranchName::new("feat/alpha"),
            ),
            &os_user("x"),
            "w",
        );
        let _ = subject.claim(
            &BranchTarget::new(
                UpstreamName::new("two"),
                crate::ids::BranchName::new("feat/alpha"),
            ),
            &os_user("y"),
            "w",
        );
        let only = subject.claims(Some(&UpstreamName::new("one")));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].repo, "one");
    }

    #[test]
    fn each_statement_on_the_ledger_answers_through_its_accessor() {
        // Given: a ledger beside the state file holding one statement of each kind
        let dir = tempfile::tempdir().unwrap();
        let at = |second: u8| format!("2026-08-15T22:14:0{second}Z");
        state(
            dir.path(),
            &at(1),
            "feat/alpha",
            StatementKind::Pull,
            Some("4545"),
        );
        state(
            dir.path(),
            &at(2),
            "feat/alpha",
            StatementKind::Superseded,
            Some("feat/replacement"),
        );
        state(
            dir.path(),
            &at(3),
            "feat/alpha",
            StatementKind::Depends,
            Some("a-repo#7,sibling#49"),
        );
        state(
            dir.path(),
            &at(4),
            "feat/ci-only",
            StatementKind::ForkOnly,
            Some("CI we want here but not upstream"),
        );

        // When: the store opens for this repository and one with no ledger
        let other = UpstreamName::new("other-repo");
        let subject = Store::open(
            dir.path().join("state.json"),
            &crate::config::FormerNames::new(),
            &[&repo(), &other],
        )
        .unwrap();

        // Then: each accessor answers from the ledger
        assert_eq!(subject.tracked_pull(&target()), Some(4545));
        assert_eq!(subject.superseded_by(&target()), Some("feat/replacement"));
        assert_eq!(
            subject.dependencies(&target()),
            [
                Requirement::Upstream {
                    repo: repo(),
                    number: 7
                },
                Requirement::Upstream {
                    repo: UpstreamName::new("sibling"),
                    number: 49
                },
            ]
        );
        assert!(subject.is_fork_only(&BranchTarget::new(repo(), BranchName::new("feat/ci-only"))));
        assert!(!subject.is_fork_only(&target()));

        // And: the same branch name in a repository nobody stated anything about
        // states nothing
        let elsewhere = BranchTarget::new(other, BranchName::new("feat/alpha"));
        assert_eq!(subject.tracked_pull(&elsewhere), None);
        assert_eq!(subject.superseded_by(&elsewhere), None);
        assert_eq!(subject.dependencies(&elsewhere), Vec::<Requirement>::new());
        assert!(!subject.is_fork_only(&elsewhere));
    }

    #[test]
    #[should_panic(
        expected = "asked about other-repo's branch statements, but the store was opened without other-repo's ledger"
    )]
    fn asking_about_a_repository_the_store_was_not_opened_for_is_a_bug() {
        // Answering "no statement" here would hide the bug behind a wrong
        // report: a fork-only branch shown as one missing its pull request.
        let dir = tempfile::tempdir().unwrap();
        let _ = store(dir.path()).is_fork_only(&BranchTarget::new(
            UpstreamName::new("other-repo"),
            BranchName::new("feat/alpha"),
        ));
    }

    #[test]
    fn a_stated_pull_request_answers_until_a_newer_forget() {
        // The case that motivated stating: a pull request opened before this tool
        // existed, then closed because the maintainer wanted a different approach.
        // Inference looks only at open pull requests from our own fork, so it can
        // never find it.
        let dir = tempfile::tempdir().unwrap();
        state(
            dir.path(),
            "2026-08-15T22:14:01Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("4545"),
        );
        assert_eq!(store(dir.path()).tracked_pull(&target()), Some(4545));

        state(
            dir.path(),
            "2026-08-15T22:14:02Z",
            "feat/alpha",
            StatementKind::Pull,
            None,
        );
        assert_eq!(store(dir.path()).tracked_pull(&target()), None);
    }

    #[test]
    fn a_ledger_the_store_cannot_read_fails_the_open() {
        // A statement the store could not read must not answer as no statement:
        // a fork-only branch would then read as one missing its pull request.
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger").join("a-repo");
        std::fs::create_dir_all(&ledger).unwrap();
        std::fs::write(
            ledger.join("20260815T221403.000000000Z-0000.md"),
            "not a ledger entry at all\n",
        )
        .unwrap();
        let path = dir.path().join("state.json");

        let read =
            Store::open(path.clone(), &crate::config::FormerNames::new(), &[&repo()]).unwrap_err();
        assert!(
            matches!(read, StoreError::Ledger(LedgerError::Parse { .. })),
            "was: {read}"
        );
        let write = Store::open_for_update(path, &crate::config::FormerNames::new(), &[&repo()])
            .unwrap_err();
        assert!(matches!(write, StoreError::Ledger(_)), "was: {write}");
    }

    #[test]
    fn only_the_ledgers_of_the_repositories_named_are_read() {
        // A ledger costs a read of its whole history, so a command about one fork
        // reads only that fork's. Here every other ledger would fail the open.
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("ledger").join("other-repo");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("20260815T221403.000000000Z-0000.md"),
            "not a ledger entry at all\n",
        )
        .unwrap();
        state(
            dir.path(),
            "2026-08-15T22:14:01Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("4545"),
        );

        assert_eq!(store(dir.path()).tracked_pull(&target()), Some(4545));
        assert!(Store::open(dir.path().join("state.json"), &FormerNames::new(), &[]).is_ok());
    }

    #[test]
    fn a_store_reads_the_ledger_beside_its_own_state_file() {
        // The default ledger is the config home's. A store opened anywhere else
        // must not read it, or a test would answer from the developer's real
        // ledgers instead of its own fixture.
        let _lock = crate::config::test_support::environment_lock();
        let environment =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        let home = tempfile::tempdir().unwrap();
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        state(
            home.path(),
            "2026-08-15T22:14:01Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("1157"),
        );

        assert_eq!(
            Store::open(default_state_path(), &FormerNames::new(), &[&repo()])
                .unwrap()
                .tracked_pull(&target()),
            Some(1157),
            "the default store reads the config home's ledger"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(store(elsewhere.path()).tracked_pull(&target()), None);
    }

    #[test]
    fn a_state_file_still_holding_the_older_statement_maps_is_refused_naming_the_fix() {
        // Opened as this knives, it would answer "no statement" for every one
        // of them, which reads as a clean slate. Both opens refuse, naming the
        // maps and the command that moves them; only the migration opens it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"tracked_pulls":{"a-repo/feat/alpha":4545},"fork_only":{"a-repo/feat/alpha":"why"},"superseded":{"a-repo/feat/alpha":"feat/replacement"},"dependencies":{"a-repo/feat/alpha":["a-repo#7"]}}"#,
        )
        .unwrap();

        for error in [
            Store::open(path.clone(), &crate::config::FormerNames::new(), &[&repo()]).unwrap_err(),
            Store::open_for_update(path.clone(), &crate::config::FormerNames::new(), &[])
                .unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(
                matches!(error, StoreError::Unmigrated { .. }),
                "was: {message}"
            );
            assert!(
                message.contains("`knives ledger migrate`"),
                "was: {message}"
            );
            assert!(
                message.contains("tracked_pulls, fork_only, superseded, dependencies"),
                "was: {message}"
            );
        }
        let mut migrating = Store::open_to_migrate(path.clone(), None).unwrap();
        assert!(migrating.drop_legacy_statements());
        migrating.save().unwrap();
        drop(migrating);
        assert!(Store::open(path, &FormerNames::new(), &[&repo()]).is_ok());
    }

    #[test]
    fn unknown_keys_are_preserved_on_rewrite() {
        // Given: state written by a newer version carrying a key we do not know
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"claims":{},"from_the_future":{"k":"v"}}"#).unwrap();
        // When: an older binary loads, changes, and saves it
        let mut subject =
            Store::open(path.clone(), &crate::config::FormerNames::new(), &[]).unwrap();
        let _ = subject.claim(&target(), &os_user("x"), "w");
        subject.save().unwrap();
        // Then: the unknown key is still there
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("from_the_future"), "state was: {text}");
    }

    #[test]
    fn the_write_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        let _ = subject.claim(&target(), &os_user("x"), "w");
        subject.save().unwrap();
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
            })
            .collect();
        names.sort();
        // The save's lock file stays: it is the inode every writer locks.
        assert_eq!(names, ["state.json", "state.lock"]);
    }

    #[test]
    fn foreign_parent_numbers_are_scoped_to_their_repo() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        subject.record_foreign_parent(&repo(), 4677, "maintainer's fix, we carry it");
        subject.record_foreign_parent(&UpstreamName::new("other"), 99, "unrelated");
        assert_eq!(subject.foreign_parent_numbers(&repo()), [4677]);
    }

    #[test]
    fn pull_heads_record_movement_between_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        subject.record_pull_head(&repo(), 42, "aaaa");
        subject.save().unwrap();
        assert_eq!(
            store(dir.path())
                .pull_heads(&repo())
                .get("42")
                .map(String::as_str),
            Some("aaaa")
        );
    }

    #[test]
    fn a_comment_mark_round_trips_and_is_scoped_to_its_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let mut store =
                Store::open_for_update(path.clone(), &crate::config::FormerNames::new(), &[])
                    .unwrap();
            store.record_comment_mark(&UpstreamName::new("a-repo"), 7, "2026-07-30T00:00:00Z");
            store.save().unwrap();
        }
        let store = Store::open(path, &crate::config::FormerNames::new(), &[]).unwrap();
        assert_eq!(
            store.comment_mark(&UpstreamName::new("a-repo"), 7),
            Some("2026-07-30T00:00:00Z")
        );
        assert_eq!(
            store.comment_mark(&UpstreamName::new("other-repo"), 7),
            None
        );
    }

    #[test]
    fn pull_states_round_trip_and_are_scoped_to_their_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let mut subject =
                Store::open_for_update(path.clone(), &crate::config::FormerNames::new(), &[])
                    .unwrap();
            subject.record_pull_state(&UpstreamName::new("a-repo"), 7, ForgeState::Merged);
            subject.save().unwrap();
        }
        let subject = Store::open(path, &crate::config::FormerNames::new(), &[]).unwrap();
        assert_eq!(
            subject.pull_state(&UpstreamName::new("a-repo"), 7),
            Some("MERGED")
        );
        assert_eq!(
            subject.pull_state(&UpstreamName::new("other-repo"), 7),
            None
        );
    }

    #[test]
    fn a_reader_is_never_blocked_by_a_writer() {
        // Reading cannot lose a write, so a held fork lock keeps out writers only.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let writer = Store::open_for_update(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();

        assert!(Store::open(path.clone(), &FormerNames::new(), &[&repo()]).is_ok());

        drop(writer);
        assert!(
            Store::open_for_update(path, &FormerNames::new(), &[&repo()]).is_ok(),
            "the lock outlived its holder"
        );
    }

    #[test]
    fn a_writer_about_another_fork_does_not_wait() {
        // Given: a writer holding a-repo for as long as it likes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let _held = Store::open_for_update(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();

        // When: a writer about another fork opens, and saves.
        let started = std::time::Instant::now();
        let other = UpstreamName::new("acme/other.repo");
        let mut subject = Store::open_for_update(path, &FormerNames::new(), &[&other])
            .expect("another fork's writer opens");
        subject.record_pull_state(&other, 7, ForgeState::Open);
        subject.save().unwrap();

        // Then: neither waited on a-repo's holder, and the lock file is named
        // for the whole upstream name.
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(dir.path().join("fork-locks/acme/other.repo.lock").is_file());
    }

    #[test]
    fn a_writer_about_the_same_fork_waits_for_the_holder() {
        // Given: a writer about a-repo that lets go after a pause.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut first =
            Store::open_for_update(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();
        let started = std::time::Instant::now();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let _ = first.claim(&target(), &os_user("first"), "w");
            first.save().unwrap();
        });

        // When: a second writer about a-repo opens.
        let second = Store::open_for_update(path, &FormerNames::new(), &[&repo()]).unwrap();
        holder.join().unwrap();

        // Then: it opened only once the first was done, and reads its claim.
        assert!(started.elapsed() >= std::time::Duration::from_millis(150));
        assert_eq!(second.claims(None)[0].owner, "first");
    }

    #[test]
    fn branch_locks_of_two_branches_are_apart_and_one_branch_has_one_holder() {
        // Given: feat/alpha's and fix/v1's locks held, and a wait that gives
        // up at its first try, so a lock someone holds reads as held at once.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let once = LockWait {
            deadline: std::time::Duration::ZERO,
            ..LockWait::CLAIM
        };
        let alpha = BranchName::new("feat/alpha");
        let held = BranchLock::acquire_at(&path, &repo(), &alpha, once).unwrap();
        let _v1 = BranchLock::acquire_at(&path, &repo(), &BranchName::new("fix/v1"), once).unwrap();

        // When / Then: another branch's lock is free, one whose name is
        // fix/v1's with a dot and more after it included, and so is the
        // fork's lock.
        let _beta = BranchLock::acquire_at(&path, &repo(), &BranchName::new("feat/beta"), once)
            .expect("feat/beta's lock is free while feat/alpha's is held");
        let _v12 = BranchLock::acquire_at(&path, &repo(), &BranchName::new("fix/v1.2"), once)
            .expect("fix/v1.2's lock is free while fix/v1's is held");
        assert!(
            FileLock::try_acquire(&dir.path().join("fork-locks/a-repo.fork"))
                .unwrap()
                .is_some(),
            "a branch's lock took the fork's"
        );
        assert!(
            dir.path()
                .join("branch-locks/a-repo/feat-beta.lock")
                .is_file()
        );

        // And: feat/alpha's lock is held until its holder lets it go, and it
        // is feat-alpha's too, whose workspace is feat/alpha's.
        let refused = BranchLock::acquire_at(&path, &repo(), &alpha, once)
            .expect_err("one branch's lock has one holder");
        assert!(matches!(refused, LockError::Held { .. }), "{refused:?}");
        let shared = BranchLock::acquire_at(&path, &repo(), &BranchName::new("feat-alpha"), once)
            .expect_err("feat-alpha shares feat/alpha's workspace, and so its lock");
        assert!(matches!(shared, LockError::Held { .. }), "{shared:?}");
        // A name with `.lock` in a segment is a file of its own beside the
        // others, not a directory under one.
        let _dotted =
            BranchLock::acquire_at(&path, &repo(), &BranchName::new("feat/alpha.lock/b"), once)
                .expect("feat/alpha.lock/b's lock is its own");
        // A workspace name with no room left for `.lock` in a file name has
        // a lock of its own too, named by its digest.
        let long = BranchName::new(format!("feat/{}", "a".repeat(249)).as_str());
        let _long = BranchLock::acquire_at(&path, &repo(), &long, once)
            .expect("a 254-byte workspace name's lock");
        assert!(
            BranchLock::acquire_at(&path, &repo(), &long, once).is_err(),
            "the long name's lock has one holder"
        );
        // And: a branch spelled as that digest's name is a lock of its own.
        let digest: [u8; 32] = {
            use sha2::Digest as _;
            sha2::Sha256::digest(format!("feat-{}", "a".repeat(249)).as_bytes()).into()
        };
        let spelled: String = std::iter::once("sha256-".to_owned())
            .chain(digest.iter().map(|byte| format!("{byte:02x}")))
            .collect();
        BranchLock::acquire_at(&path, &repo(), &BranchName::new(spelled.as_str()), once)
            .expect("a branch spelled as a digest's lock name has a lock of its own");
        drop(held);
        BranchLock::acquire_at(&path, &repo(), &alpha, once).expect("taken once let go");
    }

    #[test]
    fn no_sync_or_branch_lock_is_another_forks_lock() {
        // Given: `demo`'s sync lock and the lock of its branch `x` held, and
        // a wait that gives up at its first try.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let once = LockWait {
            deadline: std::time::Duration::ZERO,
            ..LockWait::CLAIM
        };
        let demo = UpstreamName::new("demo");
        let _sync = SyncLock::acquire_beside(&path, &demo).unwrap();
        let _branch = BranchLock::acquire_at(&path, &demo, &BranchName::new("x"), once).unwrap();

        // When / Then: the forks whose names spell those locks' paths under
        // the fork locks' root take their own locks, and so does the
        // upstream whose name spells `demo`'s with its `/` encoded.
        for other in ["demo.sync/sync", "demo.branches/x", "demo/x", "demo%2Fx"] {
            let _fork = ForkLock::acquire_beside(&path, &UpstreamName::new(other))
                .unwrap_or_else(|error| panic!("{other}'s fork lock: {error:?}"));
            let _its_sync = SyncLock::acquire_beside(&path, &UpstreamName::new(other))
                .unwrap_or_else(|error| panic!("{other}'s sync lock: {error:?}"));
        }
        BranchLock::acquire_at(
            &path,
            &UpstreamName::new("demo%2F"),
            &BranchName::new("x"),
            once,
        )
        .expect("another upstream's branch x");
    }

    #[test]
    fn an_upstream_too_long_for_a_file_name_has_locks_of_its_own() {
        // Given: two upstreams whose names, `/` encoded, pass 255 bytes, and
        // one whose name starts with `.`, and a wait that gives up at its
        // first try.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let once = LockWait {
            deadline: std::time::Duration::ZERO,
            ..LockWait::CLAIM
        };
        let first = UpstreamName::new(format!("acme/{}", "r".repeat(250)).as_str());
        let second = UpstreamName::new(format!("acme/{}", "s".repeat(250)).as_str());
        let dotted = UpstreamName::new(".dotted");

        // When / Then: each takes its sync lock and a branch's lock, the
        // first's held while the second's are taken.
        let _first_sync = SyncLock::acquire_beside(&path, &first).expect("the first's sync lock");
        let _first_branch = BranchLock::acquire_at(&path, &first, &BranchName::new("x"), once)
            .expect("the first's branch lock");
        let _second_sync =
            SyncLock::acquire_beside(&path, &second).expect("the second's sync lock");
        BranchLock::acquire_at(&path, &second, &BranchName::new("x"), once)
            .expect("the second's branch lock");
        let _dotted_sync = SyncLock::acquire_beside(&path, &dotted).expect("the dotted sync lock");
        assert!(dir.path().join("sync-locks/%2Edotted.lock").is_file());
    }

    #[test]
    fn a_locked_reread_reads_what_another_writer_saved_and_holds_the_forks_lock() {
        // Given: a writer that opened the store with no lock and made a
        // change, and another writer's claim saved after that open.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut subject = Store::open(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();
        subject.record_pull_head(&repo(), 42, "aaaa");
        let mut other = Store::open(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();
        let _ = other.claim(&target(), &os_user("other"), "w");
        other.save().unwrap();
        assert!(subject.claim_of(&target()).is_none());

        // When: the writer takes the fork's lock and reads again.
        let lock = subject.reread_locked().unwrap();

        // Then: it answers with the other writer's claim and its own change,
        // and the fork's lock is held until the lock it got goes.
        assert_eq!(subject.claim_of(&target()).unwrap().owner, "other");
        assert_eq!(subject.pull_heads(&repo()).get("42").unwrap(), "aaaa");
        let fork = dir.path().join("fork-locks/a-repo.fork");
        assert!(FileLock::try_acquire(&fork).unwrap().is_none());
        drop(lock);
        assert!(FileLock::try_acquire(&fork).unwrap().is_some());

        // And: a store that holds the fork's lock since its open reads again
        // without taking it a second time, which would wait on itself.
        let mut holder = Store::open_for_update(path, &FormerNames::new(), &[&repo()]).unwrap();
        let _again = holder.reread_locked().unwrap();
        assert!(FileLock::try_acquire(&fork).unwrap().is_none());
        subject.save().unwrap();
        assert_eq!(
            store(dir.path()).pull_heads(&repo()).get("42").unwrap(),
            "aaaa"
        );
    }

    #[test]
    fn interleaved_writers_about_two_forks_lose_neither_change() {
        // Given: two writers about two forks, both open before either saves.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let other = UpstreamName::new("other-repo");
        let mut first =
            Store::open_for_update(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();
        let mut second =
            Store::open_for_update(path.clone(), &FormerNames::new(), &[&other]).unwrap();
        let _ = first.claim(&target(), &os_user("first"), "w");
        second.record_pull_state(&other, 7, ForgeState::Merged);
        second.record_comment_mark(&other, 7, "2026-11-02T10:00:00Z");

        // When: both save, the one that read first saving first.
        first.save().unwrap();
        second.save().unwrap();

        // Then: the file has both, and the later saver now answers for both.
        let reread = Store::open(path, &FormerNames::new(), &[]).unwrap();
        assert_eq!(reread.claims(None).len(), 1);
        assert_eq!(reread.pull_state(&other, 7), Some("MERGED"));
        assert_eq!(second.claims(None).len(), 1);
    }

    #[test]
    fn a_claim_moved_since_it_was_read_fails_the_save_and_writes_nothing() {
        // Given: two writers that bypassed the fork's lock, as an older knives
        // would, each deciding a-repo/feat/alpha is free.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut first = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let mut second = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let _ = first.claim(&target(), &os_user("first"), "w");
        let _ = second.claim(&target(), &os_user("second"), "w");
        second.record_comment_mark(&repo(), 7, "2026-11-02T10:00:00Z");
        first.save().unwrap();

        // When: the second saves.
        let error = second.save().unwrap_err();

        // Then: refused, naming the holder, and nothing of it written.
        assert!(
            matches!(&error, StoreError::ClaimMoved { now, .. } if now == "now held by first"),
            "was: {error}"
        );
        let reread = Store::open(path, &FormerNames::new(), &[]).unwrap();
        assert_eq!(reread.claims(None)[0].owner, "first");
        assert_eq!(reread.comment_mark(&repo(), 7), None);
    }

    #[test]
    fn the_owner_that_moved_a_claim_is_named_escaped_and_kept_raw_in_the_file() {
        // Given: the writer that moved the claim put control characters in
        // its own owner name. The refusal is printed to a terminal, so raw
        // that writer would choose what the losing caller's terminal
        // receives; the state file stays the raw record. C1 controls count:
        // U+009B is CSI and U+009D is OSC.
        let hostile = "first-\u{1b}]0;x\u{7}\u{9b}31m\u{9d}2;y";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut first = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let mut second = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let _ = first.claim(&target(), &os_user(hostile), "w");
        let _ = second.claim(&target(), &os_user("second"), "w");
        first.save().unwrap();

        // When: the second saves over the claim it read as free.
        let error = second.save().unwrap_err();

        // Then: the message names that owner with every control character of
        // it as U+FFFD, and the file holds the name as it was written.
        assert!(
            matches!(
                &error,
                StoreError::ClaimMoved { now, .. }
                    if now == "now held by first-\u{fffd}]0;x\u{fffd}\u{fffd}31m\u{fffd}2;y"
            ),
            "was: {error}"
        );
        assert!(
            !error
                .to_string()
                .contains(['\u{1b}', '\u{7}', '\u{9b}', '\u{9d}']),
            "a control character reached the terminal: {error}"
        );
        let reread = Store::open(path, &FormerNames::new(), &[]).unwrap();
        assert_eq!(reread.claims(None)[0].owner, hostile);
    }

    #[test]
    fn a_release_of_a_claim_moved_since_it_was_read_is_refused() {
        // Given: a claim, read by a writer that will release it, then seized
        // by a writer that bypassed the fork's lock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut first = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let _ = first.claim(&target(), &os_user("first"), "w");
        first.save().unwrap();
        let mut releasing = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let mut seizing = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let _ = seizing.claim(&target(), &os_user("seizer"), "w");
        seizing.save().unwrap();

        // When: the release saves.
        assert!(releasing.release_claim(&target()));
        let error = releasing.save().unwrap_err();

        // Then: the seizer's claim stands.
        assert!(
            matches!(error, StoreError::ClaimMoved { .. }),
            "was: {error}"
        );
        let reread = Store::open(path, &FormerNames::new(), &[]).unwrap();
        assert_eq!(reread.claims(None)[0].owner, "seizer");
    }

    #[test]
    fn a_restated_reason_over_a_claim_moved_since_it_was_read_is_refused() {
        // Given: a claim, read by the writer about to say why it is held,
        // then seized by a writer that bypassed the fork's lock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut first = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let _ = first.claim(&target(), &os_user("first"), "port it");
        first.save().unwrap();
        let mut resuming = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let mut seizing = Store::open(path.clone(), &FormerNames::new(), &[]).unwrap();
        let _ = seizing.claim(&target(), &os_user("seizer"), "rescue it");
        seizing.save().unwrap();

        // When: the resume saves its new reason.
        assert!(
            resuming
                .restate_claim(&target(), "finish the port")
                .is_some()
        );
        let error = resuming.save().unwrap_err();

        // Then: the seizer's claim stands, reason and all.
        assert!(
            matches!(error, StoreError::ClaimMoved { .. }),
            "was: {error}"
        );
        let reread = Store::open(path, &FormerNames::new(), &[]).unwrap();
        assert_eq!(reread.claims(None)[0].owner, "seizer");
        assert_eq!(reread.claims(None)[0].why, "rescue it");
    }

    #[test]
    fn a_save_with_nothing_changed_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut subject =
            Store::open_for_update(path.clone(), &FormerNames::new(), &[&repo()]).unwrap();
        subject.save().unwrap();
        assert!(!path.exists());
        assert!(!dir.path().join("state.lock").exists());
    }
}
