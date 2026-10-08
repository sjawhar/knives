//! The things no amount of computing can recover.
//!
//! Detectors are cheap and local, so nothing derived is cached here. What lives
//! here is intent: who is working on what and why, and why we carry someone
//! else's pull request as a release parent.
//!
//! What a person stated about a branch — its pull request, that it has none
//! upstream on purpose, what superseded it, what it cannot land before — lives on
//! the ledger entry that recorded the statement. The store reads those back when
//! it opens and answers for them, so a reader asks the store either way.
//!
//! Intent cannot be inferred from the repository, and it cannot be inferred from
//! session working directories either: an agent launched elsewhere may need to
//! change a fork.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::default_config_path;
use crate::ids::{BranchTarget, RepoName, Requirement};
use crate::ledger::{Ledger, LedgerError};
use crate::lock::{FileLock, LockError, LockWait};
use crate::statement::Statements;

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
    /// here: nothing reads them, and nothing drops them either.
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
    /// Every repository's live statements, keyed by repository, read from the
    /// ledger once when the store opens.
    statements: BTreeMap<String, Statements>,
    /// Present only for a store opened to be written. Held, not read: its whole
    /// job is to exist until this value is dropped.
    _lock: Option<FileLock>,
}

impl Store {
    /// Read-only. Cheap, and cannot block another agent.
    ///
    /// Either open fails on a ledger it cannot read, as it does on a state file
    /// it cannot read: a statement the store could not read must not answer as
    /// no statement.
    pub fn open(path: PathBuf) -> Result<Self, StoreError> {
        Self::read(path, None)
    }

    /// For a read-modify-write. Holds the lock until dropped, and waits the
    /// full claim-writer budget ([`LockWait::CLAIM`]) for another writer.
    pub fn open_for_update(path: PathBuf) -> Result<Self, StoreError> {
        let lock = FileLock::acquire(&path, LockWait::CLAIM)?;
        Self::read(path, Some(lock))
    }

    fn read(path: PathBuf, lock: Option<FileLock>) -> Result<Self, StoreError> {
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
        let statements = read_statements(&path.with_file_name("ledger"))?;
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

    pub fn claims(&self, repo: Option<&RepoName>) -> Vec<&Claim> {
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
    pub fn is_fork_only(&self, target: &BranchTarget) -> bool {
        self.stated_in(&target.repo)
            .is_some_and(|stated| stated.fork_only(target.branch.as_str()))
    }

    pub fn record_foreign_parent(&mut self, repo: &RepoName, number: u64, why: &str) {
        let _ = self
            .state
            .foreign_parents
            .insert(format!("{repo}/{number}"), why.to_owned());
    }

    /// Pull request numbers we carry as release parents but did not author.
    ///
    /// A release parent can be any upstream pull request, including a
    /// maintainer's, so these are tracked even though no branch of ours matches.
    pub fn foreign_parent_numbers(&self, repo: &RepoName) -> Vec<u64> {
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
        self.stated_in(&target.repo)?
            .superseded_by(target.branch.as_str())
    }

    /// What `target` cannot land before.
    pub fn dependencies(&self, target: &BranchTarget) -> Vec<Requirement> {
        self.stated_in(&target.repo)
            .map_or_else(Vec::new, |stated| {
                stated
                    .depends(target.branch.as_str())
                    .iter()
                    .filter_map(|text| Requirement::parse(text))
                    .collect()
            })
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
        self.stated_in(&target.repo)?.pull(target.branch.as_str())
    }

    fn stated_in(&self, repo: &RepoName) -> Option<&Statements> {
        self.statements.get(repo.as_str())
    }

    pub fn convention_digest(&self, repo: &RepoName, file: &str) -> Option<&str> {
        self.state
            .conventions
            .get(&format!("{repo}/{file}"))
            .map(String::as_str)
    }

    pub fn record_convention_digest(&mut self, repo: &RepoName, file: &str, digest: &str) {
        let _ = self
            .state
            .conventions
            .insert(format!("{repo}/{file}"), digest.to_owned());
    }

    pub fn pull_heads(&self, repo: &RepoName) -> BTreeMap<String, String> {
        self.state
            .pull_heads
            .get(repo.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn record_pull_head(&mut self, repo: &RepoName, number: u64, sha: &str) {
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
    pub fn record_pull_state(&mut self, repo: &RepoName, number: u64, state: ForgeState) {
        let _ = self
            .state
            .pull_states
            .insert(format!("{repo}#{number}"), state.as_recorded().to_owned());
    }

    /// Drop a record this version cannot read, so its problem is reported once.
    pub fn forget_pull_state(&mut self, repo: &RepoName, number: u64) {
        let _ = self.state.pull_states.remove(&format!("{repo}#{number}"));
    }

    /// The recorded state as the file spells it: `record_pull_state` writes a
    /// forge state, but a file an earlier version wrote may spell otherwise, and
    /// the reader decides what that means.
    pub fn pull_state(&self, repo: &RepoName, number: u64) -> Option<&str> {
        self.state
            .pull_states
            .get(&format!("{repo}#{number}"))
            .map(String::as_str)
    }

    pub fn record_comment_mark(&mut self, repo: &RepoName, number: u64, at: &str) {
        let _ = self
            .state
            .comment_marks
            .insert(format!("{repo}#{number}"), at.to_owned());
    }

    pub fn comment_mark(&self, repo: &RepoName, number: u64) -> Option<&str> {
        self.state
            .comment_marks
            .get(&format!("{repo}#{number}"))
            .map(String::as_str)
    }
}

/// Every repository's live statements, from the ledgers beside the state file.
///
/// `ledger/<repo>/` beside `state.json` is where
/// [`crate::ledger::default_ledger_path`] puts each repository's ledger, so a
/// store opened at the default path reads the default ledgers and one opened
/// anywhere else reads only what sits beside it. A ledger root that does not
/// exist yet holds no statements. Hidden directories, such as the ledger's own
/// `.git`, anything that is not a directory, and a name that is not UTF-8 are
/// not a repository's ledger: knives writes none of them.
fn read_statements(root: &Path) -> Result<BTreeMap<String, Statements>, StoreError> {
    let unreadable = |path: &Path, source| StoreError::Read {
        path: path.to_owned(),
        source,
    };
    let listing = match std::fs::read_dir(root) {
        Ok(listing) => listing,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BTreeMap::new());
        }
        Err(source) => return Err(unreadable(root, source)),
    };
    let mut statements = BTreeMap::new();
    for dirent in listing {
        let path = dirent.map_err(|source| unreadable(root, source))?.path();
        let Some(repo) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if repo.starts_with('.')
            || !std::fs::metadata(&path)
                .map_err(|source| unreadable(&path, source))?
                .is_dir()
        {
            continue;
        }
        let entries = Ledger::at(path.clone()).entries()?;
        let _ = statements.insert(repo.to_owned(), Statements::from_entries(&entries));
    }
    Ok(statements)
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

    fn store(dir: &Path) -> Store {
        Store::open(dir.join("state.json")).unwrap()
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

    fn repo() -> RepoName {
        RepoName::new("a-repo")
    }

    fn target() -> BranchTarget {
        BranchTarget::new(
            RepoName::new("a-repo"),
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
        let mut first = Store::open(path.clone()).unwrap();
        let _ = first.claim(&target(), &identity, "fixing the parser");
        first.save().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(r#""kind": "harness-session""#),
            "state was: {text}"
        );

        let reloaded = Store::open(path).unwrap();
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

        let store = Store::open(path).unwrap();
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
                RepoName::new("one"),
                crate::ids::BranchName::new("feat/alpha"),
            ),
            &os_user("x"),
            "w",
        );
        let _ = subject.claim(
            &BranchTarget::new(
                RepoName::new("two"),
                crate::ids::BranchName::new("feat/alpha"),
            ),
            &os_user("y"),
            "w",
        );
        let only = subject.claims(Some(&RepoName::new("one")));
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

        // When: the store opens
        let subject = store(dir.path());

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
                    repo: RepoName::new("sibling"),
                    number: 49
                },
            ]
        );
        assert!(subject.is_fork_only(&BranchTarget::new(repo(), BranchName::new("feat/ci-only"))));
        assert!(!subject.is_fork_only(&target()));

        // And: the same branch name in another repository states nothing
        let elsewhere =
            BranchTarget::new(RepoName::new("other-repo"), BranchName::new("feat/alpha"));
        assert_eq!(subject.tracked_pull(&elsewhere), None);
        assert_eq!(subject.superseded_by(&elsewhere), None);
        assert!(subject.dependencies(&elsewhere).is_empty());
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

        let read = Store::open(path.clone()).unwrap_err();
        assert!(
            matches!(read, StoreError::Ledger(LedgerError::Parse { .. })),
            "was: {read}"
        );
        let write = Store::open_for_update(path).unwrap_err();
        assert!(matches!(write, StoreError::Ledger(_)), "was: {write}");
    }

    #[test]
    fn only_a_repository_ledger_directory_is_read() {
        // The ledger root is a git repository in practice, and an editor or a sync
        // tool can leave a file beside the repositories' directories. Each of these
        // holds a file that would fail to parse as an entry.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ledger");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git").join("HEAD.md"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("README.md"), "not a repository's ledger\n").unwrap();
        state(
            dir.path(),
            "2026-08-15T22:14:01Z",
            "feat/alpha",
            StatementKind::Pull,
            Some("4545"),
        );

        assert_eq!(store(dir.path()).tracked_pull(&target()), Some(4545));
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
            Store::open(default_state_path())
                .unwrap()
                .tracked_pull(&target()),
            Some(1157),
            "the default store reads the config home's ledger"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        assert_eq!(store(elsewhere.path()).tracked_pull(&target()), None);
    }

    #[test]
    fn statements_an_older_version_kept_in_the_state_file_ride_along_unread() {
        // Nothing reads them now, and a save must not drop them either: they are
        // the record a later migration moves onto the ledger.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"tracked_pulls":{"a-repo/feat/alpha":4545},"fork_only":{"a-repo/feat/alpha":"why"},"superseded":{"a-repo/feat/alpha":"feat/replacement"},"dependencies":{"a-repo/feat/alpha":["a-repo#7"]}}"#,
        )
        .unwrap();

        let mut subject = Store::open(path.clone()).unwrap();
        assert_eq!(subject.tracked_pull(&target()), None);
        assert!(!subject.is_fork_only(&target()));
        assert_eq!(subject.superseded_by(&target()), None);
        assert!(subject.dependencies(&target()).is_empty());

        let _ = subject.claim(&target(), &os_user("x"), "w");
        subject.save().unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["tracked_pulls"]["a-repo/feat/alpha"], 4545);
        assert_eq!(saved["fork_only"]["a-repo/feat/alpha"], "why");
        assert_eq!(saved["superseded"]["a-repo/feat/alpha"], "feat/replacement");
        assert_eq!(
            saved["dependencies"]["a-repo/feat/alpha"],
            serde_json::json!(["a-repo#7"])
        );
    }

    #[test]
    fn unknown_keys_are_preserved_on_rewrite() {
        // Given: state written by a newer version carrying a key we do not know
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"claims":{},"from_the_future":{"k":"v"}}"#).unwrap();
        // When: an older binary loads, changes, and saves it
        let mut subject = Store::open(path.clone()).unwrap();
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
        subject.record_foreign_parent(&RepoName::new("other"), 99, "unrelated");
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
            let mut store = Store::open_for_update(path.clone()).unwrap();
            store.record_comment_mark(&RepoName::new("a-repo"), 7, "2026-07-30T00:00:00Z");
            store.save().unwrap();
        }
        let store = Store::open(path).unwrap();
        assert_eq!(
            store.comment_mark(&RepoName::new("a-repo"), 7),
            Some("2026-07-30T00:00:00Z")
        );
        assert_eq!(store.comment_mark(&RepoName::new("other-repo"), 7), None);
    }

    #[test]
    fn pull_states_round_trip_and_are_scoped_to_their_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let mut subject = Store::open_for_update(path.clone()).unwrap();
            subject.record_pull_state(&RepoName::new("a-repo"), 7, ForgeState::Merged);
            subject.save().unwrap();
        }
        let subject = Store::open(path).unwrap();
        assert_eq!(
            subject.pull_state(&RepoName::new("a-repo"), 7),
            Some("MERGED")
        );
        assert_eq!(subject.pull_state(&RepoName::new("other-repo"), 7), None);
    }

    #[test]
    fn a_reader_is_never_blocked_by_a_writer() {
        // Reading cannot lose a write, so a held writer lock keeps out writers only.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let writer = Store::open_for_update(path.clone()).unwrap();

        assert!(Store::open(path.clone()).is_ok());

        drop(writer);
        assert!(
            Store::open_for_update(path).is_ok(),
            "the lock outlived its holder"
        );
    }
}
