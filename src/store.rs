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

use crate::config::{ConfigError, default_config_path};
use crate::ids::{BranchTarget, Requirement, UpstreamName};
use crate::ledger::{Ledger, LedgerError};
use crate::lock::{FileLock, LockError, LockWait};
use crate::statement::{Statement, StatementKind, Statements};

use crate::commands::claim::Identity;
use crate::commands::sync::ForgeState;
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

/// The maps an older knives kept branch statements in, each keyed
/// `<repo>/<branch>`: a pull request, fork-only, superseded-by and
/// depends-on.
const LEGACY_STATEMENTS: [&str; 4] = ["tracked_pulls", "fork_only", "superseded", "dependencies"];

/// One branch statement an older knives kept in the state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyStatement {
    /// `<repo>/<branch>`, as the older knives keyed it.
    pub key: String,
    pub statement: Statement,
}

/// A state-file map whose keys name a fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateMap {
    /// `<repo>/<branch>`, and the claim's own `repo`.
    Claims,
    /// `<repo>#<number>`.
    CommentMarks,
    /// `<repo>#<number>`.
    PullStates,
    /// `<repo>/<number>`.
    ForeignParents,
    /// `<repo>/<file>`.
    Conventions,
    /// `<repo>`.
    PullHeads,
}

impl std::fmt::Display for StateMap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Claims => "claims",
            Self::CommentMarks => "comment_marks",
            Self::PullStates => "pull_states",
            Self::ForeignParents => "foreign_parents",
            Self::Conventions => "conventions",
            Self::PullHeads => "pull_heads",
        })
    }
}

/// One state-file key that named a fork by the registry key an older knives
/// kept it under, and the key `knives ledger migrate` moves it to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Renamed {
    pub map: StateMap,
    pub from: String,
    pub to: String,
}

/// Each registry key an older knives kept a fork's ledger and state under,
/// with the name they are kept under now
/// ([`crate::config::Registry::former_names`]).
pub type FormerNames = BTreeMap<String, UpstreamName>;

/// Whether `directory` holds a ledger entry file of its own.
///
/// That is a `*.md` regular file directly inside it, not inside a directory
/// below it. Unreadable is taken as holding one, so a directory nobody could
/// read is refused rather than passed as migrated.
pub fn holds_entries(directory: &Path) -> bool {
    let Ok(listing) = std::fs::read_dir(directory) else {
        return directory.exists();
    };
    listing.into_iter().any(|dirent| {
        dirent.is_err()
            || dirent.is_ok_and(|dirent| {
                dirent.file_type().is_ok_and(|kind| kind.is_file())
                    && Path::new(&dirent.file_name()).extension()
                        == Some(std::ffi::OsStr::new("md"))
            })
    })
}

/// `text`, a fork's name followed by `separator` and the rest, with a former
/// name replaced by the name that fork is kept under now; `None` when `text`
/// starts with no former name.
///
/// A text that already starts with a current name is left alone, whatever
/// its first component reads: a registry key can spell another fork's owner
/// (`openchamber` is `openchamber/openchamber`), and a migrated key must not
/// be migrated again.
pub fn renamed(former: &FormerNames, text: &str, separator: char) -> Option<String> {
    let current = former.values().any(|now| {
        text.strip_prefix(now.as_str())
            .is_some_and(|rest| rest.starts_with(separator))
    });
    if current {
        return None;
    }
    let (name, rest) = text.split_once(separator)?;
    former
        .get(name)
        .map(|now| format!("{now}{separator}{rest}"))
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
    #[error(transparent)]
    Registry(#[from] ConfigError),
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

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    state: State,
    /// The live statements of each repository the store was opened for, read
    /// from its ledger once when the store opens.
    statements: BTreeMap<UpstreamName, Statements>,
    /// Present only for a store opened to be written. Held, not read: its whole
    /// job is to exist until this value is dropped.
    _lock: Option<FileLock>,
}

impl Store {
    /// Read-only. Cannot block another agent.
    ///
    /// `repos` are the repositories whose branch statements the caller will ask
    /// about, and only their ledgers are read: a ledger costs a read of its whole
    /// history, which a command about one fork must not pay for every other.
    /// Either open fails on a ledger it cannot read, as it does on a state file
    /// it cannot read: a statement the store could not read must not answer as
    /// no statement.
    pub fn open(path: PathBuf, repos: &[&UpstreamName]) -> Result<Self, StoreError> {
        Self::read(path, repos, None)?.migrated()
    }

    /// For a read-modify-write. Holds the lock until dropped, and waits the
    /// full claim-writer budget ([`LockWait::CLAIM`]) for another writer.
    /// `repos` as for [`Store::open`].
    pub fn open_for_update(path: PathBuf, repos: &[&UpstreamName]) -> Result<Self, StoreError> {
        let lock = FileLock::acquire(&path, LockWait::CLAIM)?;
        Self::read(path, repos, Some(lock))?.migrated()
    }

    /// [`Store::open_for_update`] for `knives ledger migrate` alone: it opens
    /// the state file an older knives left, which every other open refuses.
    pub fn open_to_migrate(path: PathBuf) -> Result<Self, StoreError> {
        let lock = FileLock::acquire(&path, LockWait::CLAIM)?;
        Self::read(path, &[], Some(lock))
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
    /// Which keys are former names is the registry's to say, so it is read
    /// from `repos.toml` beside the state file, as the ledger is read from
    /// beside it: a store opened anywhere else answers for what sits there.
    /// A former name's ledger directory is unmigrated while it holds an entry
    /// file; one holding only other forks' directories is the owner directory
    /// of their upstream names (`acme` beside `acme/demo`).
    fn migrated(self) -> Result<Self, StoreError> {
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
        let registry = crate::config::load(&self.path.with_file_name("repos.toml"))?;
        let former = registry.former_names();
        let root = self.path.with_file_name("ledger");
        let maps: BTreeSet<StateMap> = self
            .state
            .renames(&former)
            .into_iter()
            .map(|renamed| renamed.map)
            .collect();
        let held: Vec<String> = maps
            .iter()
            .map(ToString::to_string)
            .chain(
                former
                    .keys()
                    .map(|name| root.join(name))
                    .filter(|directory| holds_entries(directory))
                    .map(|directory| directory.display().to_string()),
            )
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
        lock: Option<FileLock>,
    ) -> Result<Self, StoreError> {
        let state = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|source| StoreError::Read {
                path: path.clone(),
                source,
            })?;
            serde_json::from_str(&text).map_err(|source| StoreError::Parse {
                path: path.clone(),
                source,
            })?
        } else {
            State::default()
        };
        let statements = read_statements(&path.with_file_name("ledger"), repos)?;
        Ok(Self {
            path,
            state,
            statements,
            _lock: lock,
        })
    }

    /// Write atomically, so a crash mid-write cannot truncate the file and a
    /// concurrent reader never sees a half-written document.
    pub fn save(&self) -> Result<(), StoreError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
            path: parent.to_owned(),
            source,
        })?;
        let mut temp =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| StoreError::Write {
                path: parent.to_owned(),
                source,
            })?;
        let text = serde_json::to_string_pretty(&self.state)?;
        temp.write_all(text.as_bytes())
            .and_then(|()| temp.write_all(b"\n"))
            .map_err(|source| StoreError::Write {
                path: self.path.clone(),
                source,
            })?;
        temp.persist(&self.path)
            .map_err(|error| StoreError::Write {
                path: self.path.clone(),
                source: error.error,
            })?;
        Ok(())
    }

    pub fn claim(&mut self, target: &BranchTarget, identity: &Identity, why: &str) -> Claim {
        let record = Claim {
            repo: target.repo.to_string(),
            branch: target.branch.to_string(),
            owner: identity.owner.clone(),
            kind: identity.kind,
            why: why.to_owned(),
            started: jiff::Timestamp::now().to_string(),
            files: Vec::new(),
        };
        let _ = self.state.claims.insert(record.key(), record.clone());
        record
    }

    pub fn release_claim(&mut self, target: &BranchTarget) -> bool {
        self.state.claims.remove(&target.to_string()).is_some()
    }

    pub fn claims(&self, repo: Option<&UpstreamName>) -> Vec<&Claim> {
        self.state
            .claims
            .values()
            .filter(|claim| repo.is_none_or(|name| claim.repo == name.as_str()))
            .collect()
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
        let _ = self
            .state
            .foreign_parents
            .insert(format!("{repo}/{number}"), why.to_owned());
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

    #[allow(
        clippy::panic,
        reason = "a store asked about a repository it was not opened for is a knives bug; answering no statement would be the silent wrong report this exists to prevent"
    )]
    fn stated_in(&self, repo: &UpstreamName) -> &Statements {
        self.statements.get(repo).unwrap_or_else(|| {
            panic!(
                "knives bug: asked about {repo}'s branch statements, but the store was opened without {repo}'s ledger"
            )
        })
    }

    pub fn convention_digest(&self, repo: &UpstreamName, file: &str) -> Option<&str> {
        self.state
            .conventions
            .get(&format!("{repo}/{file}"))
            .map(String::as_str)
    }

    pub fn record_convention_digest(&mut self, repo: &UpstreamName, file: &str, digest: &str) {
        let _ = self
            .state
            .conventions
            .insert(format!("{repo}/{file}"), digest.to_owned());
    }

    pub fn pull_heads(&self, repo: &UpstreamName) -> BTreeMap<String, String> {
        self.state
            .pull_heads
            .get(repo.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn record_pull_head(&mut self, repo: &UpstreamName, number: u64, sha: &str) {
        let _ = self
            .state
            .pull_heads
            .entry(repo.to_string())
            .or_default()
            .insert(number.to_string(), sha.to_owned());
    }

    /// Record where the forge said a pull request stands. The parameter type is
    /// the invariant: a record is a forge state, never the transition sync
    /// classified from it.
    pub fn record_pull_state(&mut self, repo: &UpstreamName, number: u64, state: ForgeState) {
        let _ = self
            .state
            .pull_states
            .insert(format!("{repo}#{number}"), state.as_recorded().to_owned());
    }

    /// Drop a record this version cannot read, so its problem is reported once.
    pub fn forget_pull_state(&mut self, repo: &UpstreamName, number: u64) {
        let _ = self.state.pull_states.remove(&format!("{repo}#{number}"));
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
        let _ = self
            .state
            .comment_marks
            .insert(format!("{repo}#{number}"), at.to_owned());
    }

    pub fn comment_mark(&self, repo: &UpstreamName, number: u64) -> Option<&str> {
        self.state
            .comment_marks
            .get(&format!("{repo}#{number}"))
            .map(String::as_str)
    }

    /// Every branch statement an older knives kept in the state file, in map
    /// order: pull requests, then fork-only, superseded-by and depends-on.
    ///
    /// A depends-on list is one statement, comma-joined as the ledger carries
    /// it; an empty list states that the branch needs nothing. A map that does
    /// not have the shape the older knives wrote fails the read, so nothing is
    /// migrated from a file nobody can vouch for.
    pub fn legacy_statements(&self) -> Result<Vec<LegacyStatement>, StoreError> {
        #[derive(Deserialize)]
        struct Legacy {
            #[serde(default)]
            tracked_pulls: BTreeMap<String, u64>,
            #[serde(default)]
            fork_only: BTreeMap<String, String>,
            #[serde(default)]
            superseded: BTreeMap<String, String>,
            #[serde(default)]
            dependencies: BTreeMap<String, Vec<String>>,
        }
        let maps: serde_json::Map<String, serde_json::Value> = LEGACY_STATEMENTS
            .iter()
            .filter_map(|name| {
                self.state
                    .extra
                    .get(*name)
                    .map(|map| ((*name).to_owned(), map.clone()))
            })
            .collect();
        let legacy: Legacy =
            serde_json::from_value(serde_json::Value::Object(maps)).map_err(|source| {
                StoreError::Parse {
                    path: self.path.clone(),
                    source,
                }
            })?;
        let stated = |key: String, kind: StatementKind, value: Option<String>| LegacyStatement {
            key,
            statement: Statement { kind, value },
        };
        Ok(legacy
            .tracked_pulls
            .into_iter()
            .map(|(key, number)| stated(key, StatementKind::Pull, Some(number.to_string())))
            .chain(
                legacy
                    .fork_only
                    .into_iter()
                    .map(|(key, why)| stated(key, StatementKind::ForkOnly, Some(why))),
            )
            .chain(legacy.superseded.into_iter().map(|(key, replacement)| {
                stated(key, StatementKind::Superseded, Some(replacement))
            }))
            .chain(legacy.dependencies.into_iter().map(|(key, required)| {
                let value = (!required.is_empty()).then(|| required.join(","));
                stated(key, StatementKind::Depends, value)
            }))
            .collect())
    }

    /// Drop the maps [`Store::legacy_statements`] reads; whether there were any.
    pub fn drop_legacy_statements(&mut self) -> bool {
        let before = self.state.extra.len();
        self.state
            .extra
            .retain(|name, _| !LEGACY_STATEMENTS.contains(&name.as_str()));
        self.state.extra.len() != before
    }

    /// Move every key that names a fork by one of its `former` names, and
    /// every claim's own `repo` with it, to the name that fork is kept under
    /// now; each move made, and each refused because its new key is already
    /// taken, in map order. A refused move leaves both keys as they were.
    pub fn rename_forks(&mut self, former: &FormerNames) -> (Vec<Renamed>, Vec<Renamed>) {
        self.state
            .renames(former)
            .into_iter()
            .partition(|renamed| self.state.apply(renamed, former))
    }
}

impl State {
    /// Every key that names a fork by one of its `former` names, with the key
    /// it moves to, in map order.
    ///
    /// A claim is renamed by the `repo` it records, not by parsing its key,
    /// since a branch name may hold a `/`.
    fn renames(&self, former: &FormerNames) -> Vec<Renamed> {
        let keyed = |map: StateMap, keys: Vec<&String>, separator: char| {
            keys.into_iter()
                .filter_map(move |key| {
                    Some(Renamed {
                        map,
                        from: key.clone(),
                        to: renamed(former, key, separator)?,
                    })
                })
                .collect::<Vec<_>>()
        };
        let claims = self.claims.iter().filter_map(|(key, claim)| {
            let now = former.get(claim.repo.as_str())?;
            Some(Renamed {
                map: StateMap::Claims,
                from: key.clone(),
                to: format!("{now}/{}", claim.branch),
            })
        });
        let pull_heads = self.pull_heads.keys().filter_map(|key| {
            Some(Renamed {
                map: StateMap::PullHeads,
                from: key.clone(),
                to: former.get(key.as_str())?.to_string(),
            })
        });
        claims
            .chain(keyed(
                StateMap::CommentMarks,
                self.comment_marks.keys().collect(),
                '#',
            ))
            .chain(keyed(
                StateMap::PullStates,
                self.pull_states.keys().collect(),
                '#',
            ))
            .chain(keyed(
                StateMap::ForeignParents,
                self.foreign_parents.keys().collect(),
                '/',
            ))
            .chain(keyed(
                StateMap::Conventions,
                self.conventions.keys().collect(),
                '/',
            ))
            .chain(pull_heads)
            .collect()
    }

    /// Move one value from `renamed.from` to `renamed.to`, the claim's own
    /// `repo` with it; `false`, moving nothing, when `to` is already taken.
    fn apply(&mut self, renamed: &Renamed, former: &FormerNames) -> bool {
        fn rekey<V>(map: &mut BTreeMap<String, V>, renamed: &Renamed) -> bool {
            if map.contains_key(&renamed.to) {
                return false;
            }
            if let Some(value) = map.remove(&renamed.from) {
                let _ = map.insert(renamed.to.clone(), value);
            }
            true
        }
        match renamed.map {
            StateMap::Claims => {
                let moved = rekey(&mut self.claims, renamed);
                if moved
                    && let Some(claim) = self.claims.get_mut(&renamed.to)
                    && let Some(now) = former.get(claim.repo.as_str())
                {
                    claim.repo = now.to_string();
                }
                moved
            }
            StateMap::CommentMarks => rekey(&mut self.comment_marks, renamed),
            StateMap::PullStates => rekey(&mut self.pull_states, renamed),
            StateMap::ForeignParents => rekey(&mut self.foreign_parents, renamed),
            StateMap::Conventions => rekey(&mut self.conventions, renamed),
            StateMap::PullHeads => rekey(&mut self.pull_heads, renamed),
        }
    }
}

/// The live statements of each of `repos`, from its ledger beside the state file.
///
/// `ledger/<owner>/<name>/` beside `state.json` is where
/// [`crate::ledger::default_ledger_path`] puts each fork's ledger, so a store
/// opened at the default path reads the default ledgers and one opened
/// anywhere else reads only what sits beside it. A repository nobody has
/// stated anything about yet has no ledger, and so no statements.
fn read_statements(
    root: &Path,
    repos: &[&UpstreamName],
) -> Result<BTreeMap<UpstreamName, Statements>, StoreError> {
    repos
        .iter()
        .map(|repo| {
            let entries = Ledger::at(root.join(repo.as_str())).entries()?;
            Ok(((*repo).clone(), Statements::from_entries(&entries)))
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
        Store::open(dir.join("state.json"), &[&repo()]).unwrap()
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
                pr: None,
                parents: Vec::new(),
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
        let mut first = Store::open(path.clone(), &[]).unwrap();
        let _ = first.claim(&target(), &identity, "fixing the parser");
        first.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#""kind": "harness-session""#),
            "state was: {text}"
        );

        let reloaded = Store::open(path, &[]).unwrap();
        let claims = reloaded.claims(None);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].why, "fixing the parser");
        assert_eq!(claims[0].kind, OwnerKind::HarnessSession);
        assert!(!claims[0].started.is_empty());
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

        let store = Store::open(path, &[]).unwrap();
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
        assert!(subject.claims(None).is_empty());
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
        let subject = Store::open(dir.path().join("state.json"), &[&repo(), &other]).unwrap();

        // Then: each accessor answers from the ledger
        assert_eq!(subject.tracked_pull(&target()), Some(4545));
        assert_eq!(subject.superseded_by(&target()), Some("feat/replacement"));
        assert_eq!(
            subject.dependencies(&target()),
            [
                Requirement {
                    repo: repo(),
                    number: 7
                },
                Requirement {
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
        assert!(subject.dependencies(&elsewhere).is_empty());
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

        let read = Store::open(path.clone(), &[&repo()]).unwrap_err();
        assert!(
            matches!(read, StoreError::Ledger(LedgerError::Parse { .. })),
            "was: {read}"
        );
        let write = Store::open_for_update(path, &[&repo()]).unwrap_err();
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
        assert!(Store::open(dir.path().join("state.json"), &[]).is_ok());
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
            Store::open(default_state_path(), &[&repo()])
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
            Store::open(path.clone(), &[&repo()]).unwrap_err(),
            Store::open_for_update(path.clone(), &[]).unwrap_err(),
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
        let mut migrating = Store::open_to_migrate(path.clone()).unwrap();
        assert!(migrating.drop_legacy_statements());
        migrating.save().unwrap();
        drop(migrating);
        assert!(Store::open(path, &[&repo()]).is_ok());
    }

    #[test]
    fn the_statements_an_older_version_kept_read_back_one_per_statement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"tracked_pulls":{"a-repo/feat/alpha":4545},"fork_only":{"a-repo/ci":"why"},"superseded":{"a-repo/old":"feat/alpha"},"dependencies":{"a-repo/feat/alpha":["a-repo#7","b#9"],"a-repo/none":[]}}"#,
        )
        .unwrap();
        let mut subject = Store::open_to_migrate(path).unwrap();

        let stated = |key: &str, kind, value: Option<&str>| LegacyStatement {
            key: key.to_owned(),
            statement: Statement {
                kind,
                value: value.map(str::to_owned),
            },
        };
        assert_eq!(
            subject.legacy_statements().unwrap(),
            [
                stated("a-repo/feat/alpha", StatementKind::Pull, Some("4545")),
                stated("a-repo/ci", StatementKind::ForkOnly, Some("why")),
                stated("a-repo/old", StatementKind::Superseded, Some("feat/alpha")),
                stated(
                    "a-repo/feat/alpha",
                    StatementKind::Depends,
                    Some("a-repo#7,b#9")
                ),
                stated("a-repo/none", StatementKind::Depends, None),
            ]
        );

        assert!(subject.drop_legacy_statements());
        assert!(subject.legacy_statements().unwrap().is_empty());
        assert!(!subject.drop_legacy_statements());
    }

    #[test]
    fn a_legacy_map_of_the_wrong_shape_fails_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"tracked_pulls":{"a-repo/feat/alpha":"soon"}}"#).unwrap();

        let error = Store::open_to_migrate(path)
            .unwrap()
            .legacy_statements()
            .unwrap_err();

        assert!(matches!(error, StoreError::Parse { .. }), "was: {error}");
    }

    #[test]
    fn unknown_keys_are_preserved_on_rewrite() {
        // Given: state written by a newer version carrying a key we do not know
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"claims":{},"from_the_future":{"k":"v"}}"#).unwrap();
        // When: an older binary loads, changes, and saves it
        let mut subject = Store::open(path.clone(), &[]).unwrap();
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
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
            })
            .collect();
        assert_eq!(names, ["state.json"]);
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
            let mut store = Store::open_for_update(path.clone(), &[]).unwrap();
            store.record_comment_mark(&UpstreamName::new("a-repo"), 7, "2026-07-30T00:00:00Z");
            store.save().unwrap();
        }
        let store = Store::open(path, &[]).unwrap();
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
            let mut subject = Store::open_for_update(path.clone(), &[]).unwrap();
            subject.record_pull_state(&UpstreamName::new("a-repo"), 7, ForgeState::Merged);
            subject.save().unwrap();
        }
        let subject = Store::open(path, &[]).unwrap();
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
        // Reading cannot lose a write, so a held writer lock keeps out writers only.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let writer = Store::open_for_update(path.clone(), &[]).unwrap();

        assert!(Store::open(path.clone(), &[]).is_ok());

        drop(writer);
        assert!(
            Store::open_for_update(path, &[]).is_ok(),
            "the lock outlived its holder"
        );
    }
}
