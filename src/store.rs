//! The things no amount of computing can recover.
//!
//! Detectors are cheap and local, so nothing derived is cached here. What lives
//! here is intent: who is working on what and why, which branches we keep with
//! no upstream pull request on purpose, why we carry someone else's pull request
//! as a release parent, and where a superseded pull request went.
//!
//! Intent cannot be inferred from the repository, and it cannot be inferred from
//! session working directories either: an agent launched elsewhere may need to
//! change a fork.

use std::collections::BTreeMap;
use std::fs::{File, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::default_config_path;
use crate::ids::{BranchTarget, RepoName, Requirement};

use crate::commands::claim::Identity;
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
    pub fork_only: BTreeMap<String, String>,
    #[serde(default)]
    pub foreign_parents: BTreeMap<String, String>,
    #[serde(default)]
    pub superseded: BTreeMap<String, String>,
    #[serde(default)]
    pub pull_heads: BTreeMap<String, BTreeMap<String, String>>,
    /// Digest of each convention file the last time we looked, so preflight can
    /// say "this changed since you last read it" rather than only "it exists".
    #[serde(default)]
    pub conventions: BTreeMap<String, String>,
    /// What a branch cannot land before. Keyed by `<repo>/<branch>`, holding
    /// `<repo>#<number>` requirements that may name any managed repo, not just this
    /// one: a change here can need a pull request in a sibling fork, and dropping the
    /// thing it needs from a release without dropping this too ships something that
    /// cannot work.
    #[serde(default)]
    pub dependencies: BTreeMap<String, Vec<String>>,
    /// A branch's pull request, stated rather than inferred. Keyed by
    /// `<repo>/<branch>`.
    ///
    /// Inference matches an open pull request from our own copy of the repository,
    /// which is right as a default and wrong as the only option. A pull request opened
    /// before this tool existed cannot be found that way; neither can one that was
    /// closed because the maintainer wanted something else, nor somebody else's that we
    /// are carrying because ours was superseded. Stating it accepts any number in any
    /// state from any author.
    #[serde(default)]
    pub tracked_pulls: BTreeMap<String, u64>,
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
    /// failing the read.
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
    #[error("{}", locked_message(path, holder.as_ref()))]
    Locked {
        path: PathBuf,
        holder: Option<LockHolder>,
    },
    #[error("serialising state: {source}")]
    Serialise {
        #[from]
        source: serde_json::Error,
    },
}

/// How long a writer waits for the lock and how it spaces its attempts.
///
/// `start` holds the lock for the whole workspace creation, so a waiter must
/// outlast several holds; pauses grow so a long hold is not polled hundreds
/// of times, and jitter keeps waiters out of lockstep.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LockWait {
    pub deadline: Duration,
    pub floor: Duration,
    pub ceiling: Duration,
}

impl Default for LockWait {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(60),
            floor: Duration::from_millis(20),
            ceiling: Duration::from_secs(2),
        }
    }
}

impl LockWait {
    /// One second, for the best-effort sidecar locks (`seen`, the hook's
    /// session state): a stale one must never stall a command or a hook event.
    pub(crate) const BRIEF: Self = Self {
        deadline: Duration::from_secs(1),
        floor: Duration::from_millis(20),
        ceiling: Duration::from_millis(200),
    };
}

/// The lock file a wait gave up on: the writer's pid when the file names one,
/// and the file's age either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockHolder {
    /// `None` for a lock file without a pid (an older binary's, or one whose
    /// holder had not written its pid yet).
    pub pid: Option<u32>,
    pub held_for: Duration,
}

fn locked_message(path: &Path, holder: Option<&LockHolder>) -> String {
    match holder {
        Some(LockHolder {
            pid: Some(pid),
            held_for,
        }) => format!(
            "another knives command (pid {pid}, holding for {}s) is holding {}; try again in a moment",
            held_for.as_secs(),
            path.display()
        ),
        Some(LockHolder {
            pid: None,
            held_for,
        }) => format!(
            "another knives command (holder unknown, lock written {}s ago) is holding {}; try again in a moment",
            held_for.as_secs(),
            path.display()
        ),
        None => format!(
            "another knives command (holder unknown) is holding {}; try again in a moment",
            path.display()
        ),
    }
}

/// Uniform in `[floor, min(ceiling, floor * 2^attempt)]`: `random` is scaled
/// onto the span rather than reduced modulo it, so `0` is the floor and
/// `u64::MAX` the cap.
fn pause(attempt: u32, wait: LockWait, random: u64) -> Duration {
    let cap = wait
        .floor
        .saturating_mul(1 << attempt.min(16))
        .min(wait.ceiling);
    let span = u64::try_from(cap.saturating_sub(wait.floor).as_nanos()).unwrap_or(u64::MAX);
    let scaled = (u128::from(random) * (u128::from(span) + 1)) >> 64;
    let jitter = u64::try_from(scaled).unwrap_or(span);
    wait.floor + Duration::from_nanos(jitter)
}

/// Jitter source. The standard library's per-process randomised hasher is
/// enough to break lockstep between waiters, and costs no dependency.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// The lock file's first token is the holder's pid; a file without one has an
/// unknown holder but still an age. `None` only when the file cannot be read.
fn read_holder(path: &Path) -> Option<LockHolder> {
    let text = std::fs::read_to_string(path).ok()?;
    let pid: Option<u32> = text
        .split_whitespace()
        .next()
        .and_then(|token| token.parse().ok());
    let held_for = std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .unwrap_or_default();
    Some(LockHolder { pid, held_for })
}

/// Held for the duration of a read-modify-write.
///
/// Without it the store is last-writer-wins: two `knives start` runs that
/// interleave read, decide and write both see "unclaimed", both report success,
/// and the second erases the first. For a tool whose stated purpose is to make
/// collisions between agents visible before they cost work, the coordination
/// record cannot itself lose writes.
///
/// The lock is the operating system's advisory lock on `state.lock`
/// ([`File::try_lock`]), held by the open file handle: the kernel releases it
/// when the holder exits, however it exits — a SIGKILL from a harness that
/// timed a command out, a Ctrl-C, a panic — so a stale lock cannot outlive a
/// crashed writer and nobody has to remove a file. The file itself is never
/// unlinked: it is the shared inode every waiter locks, and deleting it by
/// path after an unlock would put two waiters on two different inodes, each
/// holding "the" lock. After acquiring, the holder writes its pid into the
/// file so a waiter that gives up can name who it waited on; the file's
/// mtime gives the age. A knives built before this lock (exclusive-create of
/// the same path) reads the persistent file as always locked while both
/// binaries run on one machine.
#[derive(Debug)]
pub(crate) struct StoreLock {
    /// Dropping the handle releases the lock.
    #[expect(
        dead_code,
        reason = "an RAII guard is used by existing, not by being read"
    )]
    file: File,
}

impl StoreLock {
    /// Beside the file it guards, named for that file's stem: `state.json` is
    /// guarded by `state.lock`. Waits per `wait`, then gives up loudly:
    /// blocking forever would hide a holder that has hung.
    pub(crate) fn acquire(target: &Path, wait: LockWait) -> Result<Self, StoreError> {
        let path = target.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
                path: parent.to_owned(),
                source,
            })?;
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| StoreError::Write {
                path: path.clone(),
                source,
            })?;
        let started = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    let elapsed = started.elapsed();
                    if elapsed >= wait.deadline {
                        return Err(StoreError::Locked {
                            holder: read_holder(&path),
                            path,
                        });
                    }
                    let remaining = wait.deadline.saturating_sub(elapsed);
                    std::thread::sleep(pause(attempt, wait, random_u64()).min(remaining));
                    attempt += 1;
                }
                Err(TryLockError::Error(source)) => {
                    return Err(StoreError::Write { path, source });
                }
            }
        }
        // Ours now: name ourselves for whoever waits on us. A write failure is
        // not a lock failure, only a nameless holder in the give-up message.
        if file.set_len(0).is_ok() {
            let _ = writeln!(file, "{}", std::process::id());
        }
        Ok(Self { file })
    }
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    state: State,
    /// Present only for a store opened to be written. Held, not read: its whole
    /// job is to exist until this value is dropped.
    #[expect(
        dead_code,
        reason = "an RAII guard is used by existing, not by being read"
    )]
    lock: Option<StoreLock>,
}

impl Store {
    /// Read-only. Cheap, and cannot block another agent.
    pub fn open(path: PathBuf) -> Result<Self, StoreError> {
        Self::read(path, None)
    }

    /// For a read-modify-write. Holds the lock until dropped, and waits the
    /// full claim-writer budget (`LockWait::default`) for another writer.
    pub fn open_for_update(path: PathBuf) -> Result<Self, StoreError> {
        Self::open_for_update_with(path, LockWait::default())
    }

    /// [`Self::open_for_update`] with an explicit wait budget.
    fn open_for_update_with(path: PathBuf, wait: LockWait) -> Result<Self, StoreError> {
        let lock = StoreLock::acquire(&path, wait)?;
        Self::read(path, Some(lock))
    }

    fn read(path: PathBuf, lock: Option<StoreLock>) -> Result<Self, StoreError> {
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
        Ok(Self { path, state, lock })
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

    pub fn mark_fork_only(&mut self, target: &BranchTarget, why: &str) {
        let _ = self
            .state
            .fork_only
            .insert(target.to_string(), why.to_owned());
    }

    /// Without this mark, a branch we deliberately keep with no upstream pull
    /// request reads as an error in every status report, forever.
    pub fn is_fork_only(&self, target: &BranchTarget) -> bool {
        self.state.fork_only.contains_key(&target.to_string())
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

    /// Record that one branch's work continued as another.
    ///
    /// Distinguishing supersession from a staleness bot closing a live branch,
    /// and from a deliberate fork-only branch, needs intent. Intent cannot be
    /// recomputed, so it is stored.
    pub fn supersede(&mut self, target: &BranchTarget, new: &str) {
        let _ = self
            .state
            .superseded
            .insert(target.to_string(), new.to_owned());
    }

    pub fn superseded_by(&self, target: &BranchTarget) -> Option<&str> {
        self.state
            .superseded
            .get(&target.to_string())
            .map(String::as_str)
    }

    /// Record that `target` cannot land before `requirements` do.
    ///
    /// Additive and deduplicated, so declaring the same requirement twice is not an
    /// error and re-running a script does not accumulate duplicates.
    pub fn add_dependencies(&mut self, target: &BranchTarget, requirements: &[Requirement]) {
        let entry = self
            .state
            .dependencies
            .entry(target.to_string())
            .or_default();
        for requirement in requirements {
            let text = requirement.to_string();
            if !entry.contains(&text) {
                entry.push(text);
            }
        }
        entry.sort();
    }

    /// What `target` cannot land before.
    pub fn dependencies(&self, target: &BranchTarget) -> Vec<Requirement> {
        self.state
            .dependencies
            .get(&target.to_string())
            .map(|list| {
                list.iter()
                    .filter_map(|text| Requirement::parse(text))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// State that `target`'s pull request is `number`, whatever its state or author.
    pub fn track_pull(&mut self, target: &BranchTarget, number: u64) {
        let _ = self.state.tracked_pulls.insert(target.to_string(), number);
    }

    /// Stop associating `target` with a stated pull request.
    pub fn untrack_pull(&mut self, target: &BranchTarget) -> bool {
        self.state
            .tracked_pulls
            .remove(&target.to_string())
            .is_some()
    }

    /// The pull request stated for `target`, if any. Overrides inference.
    pub fn tracked_pull(&self, target: &BranchTarget) -> Option<u64> {
        self.state.tracked_pulls.get(&target.to_string()).copied()
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

    pub fn record_pull_state(&mut self, repo: &RepoName, number: u64, state: &str) {
        let _ = self
            .state
            .pull_states
            .insert(format!("{repo}#{number}"), state.to_owned());
    }

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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn store(dir: &Path) -> Store {
        Store::open(dir.join("state.json")).unwrap()
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
    fn a_stated_pull_request_survives_a_round_trip_whatever_its_state() {
        // The case that motivated it: a pull request opened before this tool existed,
        // then closed because the maintainer wanted a different approach. Inference
        // looks only at open pull requests from our own fork, so it can never find it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let target = BranchTarget::new(
            RepoName::new("ai"),
            crate::ids::BranchName::new("feat/alpha"),
        );
        {
            let mut store = Store::open_for_update(path.clone()).unwrap();
            store.track_pull(&target, 4545);
            store.save().unwrap();
        }
        let store = Store::open(path.clone()).unwrap();
        assert_eq!(store.tracked_pull(&target), Some(4545));
        {
            let mut store = Store::open_for_update(path.clone()).unwrap();
            assert!(store.untrack_pull(&target));
            store.save().unwrap();
        }
        assert_eq!(Store::open(path).unwrap().tracked_pull(&target), None);
    }

    #[test]
    fn a_fork_only_mark_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        subject.mark_fork_only(&target(), "CI we want here but not upstream");
        subject.save().unwrap();
        assert!(store(dir.path()).is_fork_only(&target()));
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
    fn supersession_records_where_the_work_went() {
        let dir = tempfile::tempdir().unwrap();
        let mut subject = store(dir.path());
        subject.supersede(&target(), "feat/replacement");
        subject.save().unwrap();
        assert_eq!(
            store(dir.path()).superseded_by(&target()),
            Some("feat/replacement")
        );
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
            subject.record_pull_state(&RepoName::new("a-repo"), 7, "merged");
            subject.save().unwrap();
        }
        let subject = Store::open(path).unwrap();
        assert_eq!(
            subject.pull_state(&RepoName::new("a-repo"), 7),
            Some("merged")
        );
        assert_eq!(subject.pull_state(&RepoName::new("other-repo"), 7), None);
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    const QUICK: LockWait = LockWait {
        deadline: Duration::from_millis(200),
        floor: Duration::from_millis(5),
        ceiling: Duration::from_millis(40),
    };

    #[test]
    fn a_second_writer_cannot_open_while_the_first_holds_the_lock() {
        // Two concurrent claim writers that interleave read, decide and write both used
        // to see "unclaimed", both report success, and the second erase the
        // first. The tool exists to make collisions visible, so its own record
        // must not lose them.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let first = Store::open_for_update_with(path.clone(), QUICK).unwrap();

        let second = Store::open_for_update_with(path.clone(), QUICK);
        assert!(
            matches!(second, Err(StoreError::Locked { .. })),
            "a second writer got in"
        );

        // A reader is never blocked: reading cannot lose a write.
        assert!(Store::open(path.clone()).is_ok());

        drop(first);
        assert!(
            Store::open_for_update_with(path, QUICK).is_ok(),
            "the lock outlived its holder"
        );
    }

    #[test]
    fn a_locked_error_names_the_holders_pid_and_age() {
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let _first = Store::open_for_update_with(path.clone(), QUICK).expect("first writer");
        let error = Store::open_for_update_with(path, QUICK).expect_err("second writer");
        let StoreError::Locked {
            holder: Some(holder),
            ..
        } = &error
        else {
            panic!("expected a Locked error with a holder, got {error:?}");
        };
        assert_eq!(holder.pid, Some(std::process::id()));
        assert!(holder.held_for < Duration::from_secs(5), "{holder:?}");
        let text = error.to_string();
        assert!(
            text.contains(&format!("pid {}", std::process::id())),
            "{text}"
        );
        assert!(text.contains("holding for"), "{text}");
    }

    #[test]
    fn a_holder_that_wrote_no_pid_reports_an_unknown_holder_and_the_locks_age() {
        // Given: something holds the OS lock on the file without naming itself.
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let nameless = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path.with_extension("lock"))
            .expect("open lock file");
        nameless.try_lock().expect("hold the lock");

        let error = Store::open_for_update_with(path, QUICK).expect_err("locked");

        let StoreError::Locked {
            holder: Some(holder),
            ..
        } = &error
        else {
            panic!("expected a Locked error with an aged holder, got {error:?}");
        };
        assert_eq!(holder.pid, None);
        assert!(holder.held_for < Duration::from_secs(5), "{holder:?}");
        let text = error.to_string();
        assert!(
            text.contains(&format!(
                "holder unknown, lock written {}s ago",
                holder.held_for.as_secs()
            )),
            "{text}"
        );
    }

    #[test]
    fn a_waiting_writer_acquires_once_the_holder_releases() {
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let held = Store::open_for_update_with(path.clone(), QUICK).expect("first writer");
        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            drop(held);
        });
        let started = std::time::Instant::now();
        let patient = LockWait {
            deadline: Duration::from_secs(2),
            ..QUICK
        };
        let _second = Store::open_for_update_with(path, patient).expect("acquired after release");
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "{:?}",
            started.elapsed()
        );
        holder.join().expect("holder thread");
    }

    /// The lock file the child-process helper below takes, when run as a child.
    const HELPER_LOCK_TARGET: &str = "KNIVES_TEST_LOCK_TARGET";

    /// Not a test: the body of the child process in
    /// [`a_lock_held_by_a_killed_process_is_free_at_once`]. Takes the lock on
    /// the target the environment names, says so on stdout, and sleeps until
    /// killed.
    #[test]
    #[ignore = "a helper run as a child process by the killed-holder test"]
    fn helper_holds_the_lock_until_killed() {
        let Some(target) = std::env::var_os(HELPER_LOCK_TARGET) else {
            return;
        };
        let _lock = StoreLock::acquire(Path::new(&target), QUICK).expect("child takes the lock");
        println!("HOLDING");
        std::io::stdout().flush().expect("flush");
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_held_by_a_killed_process_is_free_at_once() {
        // Given: another process holds the lock and is killed with SIGKILL, so
        // nothing of its own runs at exit.
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "--ignored",
                "--nocapture",
                "store::lock_tests::helper_holds_the_lock_until_killed",
            ])
            .env(HELPER_LOCK_TARGET, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the holder");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = std::io::BufReader::new(stdout).lines();
        let held = lines
            .by_ref()
            .map_while(Result::ok)
            .any(|line| line.trim() == "HOLDING");
        assert!(held, "the child never reported holding the lock");
        let blocked = Store::open_for_update_with(path.clone(), QUICK);
        assert!(
            matches!(blocked, Err(StoreError::Locked { .. })),
            "the child's lock was not seen: {blocked:?}"
        );

        // When: the holder is killed.
        child.kill().expect("SIGKILL the holder");
        let _ = child.wait();

        // Then: the lock is free at once; no file to remove, no age to judge.
        let started = std::time::Instant::now();
        let _mine = Store::open_for_update_with(path, QUICK).expect("acquired after the kill");
        assert!(
            started.elapsed() < QUICK.deadline / 2,
            "waited {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn pauses_double_to_the_ceiling_and_never_drop_below_the_floor() {
        for attempt in 0..12 {
            for random in [0, 1, u64::MAX / 3, u64::MAX] {
                let pause = pause(attempt, QUICK, random);
                let cap = QUICK
                    .floor
                    .saturating_mul(1 << attempt.min(16))
                    .min(QUICK.ceiling);
                assert!(
                    pause >= QUICK.floor && pause <= cap,
                    "attempt {attempt}: {pause:?} not in [{:?}, {cap:?}]",
                    QUICK.floor
                );
            }
        }
        assert_eq!(pause(0, QUICK, 0), QUICK.floor);
        assert_eq!(pause(10, QUICK, u64::MAX), QUICK.ceiling);
    }

    #[test]
    fn the_wait_gives_up_by_its_deadline() {
        let dir = tempfile::tempdir().expect("state dir");
        let path = dir.path().join("state.json");
        let _first = Store::open_for_update_with(path.clone(), QUICK).expect("first writer");
        let started = std::time::Instant::now();
        let _ = Store::open_for_update_with(path, QUICK).expect_err("locked");
        let waited = started.elapsed();
        assert!(
            waited >= QUICK.deadline && waited < QUICK.deadline * 3,
            "{waited:?}"
        );
    }
}
