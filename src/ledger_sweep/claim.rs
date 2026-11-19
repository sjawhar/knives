//! A claim on a shared fork, read and changed through the destination that
//! carries the fork ([`shared_fork`](super::shared_fork)).
//!
//! [`crate::ledger_git`] reads a claim ref and pushes one atomic update; what
//! it leaves to this layer is the order around them. A change appends the
//! entry that records it, commits that entry to this machine's ref as a
//! sweep would ([`filtered_pending`]), and pushes the claim and this
//! machine's ref together. It holds the ledger's lock and then the
//! destination's transport lock, the order a sweep takes them in: either
//! may wait for the other, up to a minute for the ledger's lock, and the
//! one order means the two never deadlock. Where the push applied nothing,
//! everything here goes back as it was; where its outcome is unknown,
//! nothing does, since the remote may yet apply it and only the new commit
//! keeps this machine's ref a fast-forward of what it applies.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::kept::keep_discards;
use super::{Destination, REMOTE, SharedFork, Tally, filtered_pending};
use crate::ids::CommitId;
use crate::ledger::LedgerError;
use crate::ledger_git::{
    self, ClaimPush, ClaimPushed, ClaimRecord, ClaimRef, ClaimSeen, HeldClaim, Refusal, Repository,
};
use crate::lock::{FileLock, LockWait};

/// Why a claim could not be read or changed.
///
/// git's own account, which quotes the remote, or what failed here. A
/// command names the ledger and the git directory from the [`SharedFork`]
/// it read or changed through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerFailure {
    /// What failed. To show escaped.
    pub cause: String,
}

/// The claim the ledger's remote holds at `claim`: none, a claim with its
/// record, or a claim whose record does not read, with why
/// ([`HeldClaim::record`]).
///
/// Under the destination's transport lock, so the read never meets a
/// sweep's fetch into the same git directory; the caller holds the fork's
/// lock, and this takes no ledger lock, since it commits nothing.
pub fn read_claim(
    shared: &SharedFork,
    claim: &ClaimRef,
) -> Result<Option<HeldClaim>, LedgerFailure> {
    let failed = |error: &dyn std::fmt::Display| LedgerFailure {
        cause: error.to_string(),
    };
    let _transport = FileLock::acquire(shared.destination.transport_lock(), LockWait::TRANSPORT)
        .map_err(|error| failed(&error))?;
    ledger_git::read_claim(&shared.destination.repository, REMOTE, claim)
        .map_err(|error| failed(&error))
}

/// One change of a claim on a shared fork: a take, a seize or a restate,
/// which `becomes` a [`ClaimRecord`], or a release, which `becomes`
/// [`Released`].
#[derive(Debug)]
pub struct ClaimChange<'a, B> {
    pub shared: &'a SharedFork,
    pub claim: &'a ClaimRef,
    /// The claim commit the remote must hold for the change to land, as the
    /// caller read it; `None` for a take, which lands only on no claim.
    pub expected: Option<&'a CommitId>,
    pub becomes: B,
}

/// What a claim becomes in a [`ClaimChange`]: what the claim ref is to
/// hold, written before the push, and what a change that landed carries.
pub trait Becomes {
    /// What a change that landed carries: its claim commit, or nothing.
    type Landed;

    /// Write what the claim ref is to hold, for the change the entry at
    /// `entry` records; why not, where it could not be written.
    fn write(self, repository: &Repository, entry: &Path) -> Result<Self::Landed, String>;

    /// The commit the claim ref is to hold; `None` deletes it.
    fn commit(landed: &Self::Landed) -> Option<&CommitId>;
}

/// A take, a seize or a restate: a claim commit of this record, naming the
/// file name of the entry that records the change.
impl Becomes for ClaimRecord {
    type Landed = CommitId;

    fn write(self, repository: &Repository, entry: &Path) -> Result<CommitId, String> {
        let name = entry.file_name().and_then(OsStr::to_str).ok_or_else(|| {
            format!(
                "the entry {} has no file name for a claim commit to name",
                entry.display()
            )
        })?;
        ledger_git::write_claim(repository, &self, name).map_err(|error| error.to_string())
    }

    fn commit(landed: &CommitId) -> Option<&CommitId> {
        Some(landed)
    }
}

/// A release: the claim ref goes, and a release that landed carries
/// nothing.
#[derive(Debug, Clone, Copy)]
pub struct Released;

impl Becomes for Released {
    type Landed = ();

    fn write(self, _: &Repository, _: &Path) -> Result<(), String> {
        Ok(())
    }

    fn commit((): &()) -> Option<&CommitId> {
        None
    }
}

/// What came of a [`change_claim`] whose landing carries `L`
/// ([`Becomes::Landed`]).
#[derive(Debug)]
pub struct ClaimChanged<L> {
    pub outcome: ClaimOutcome<L>,
    /// What the commit's check passed over ([`filtered_pending`]), each line
    /// naming the destination, and what could not be put back or kept: to
    /// print as a sweep prints its problems. None of them stopped the change.
    pub problems: Vec<String>,
}

/// Whether a claim's change reached the ledger's remote.
#[derive(Debug)]
pub enum ClaimOutcome<L> {
    /// The claim ref holds what the change made of it, which this carries:
    /// the claim commit of a take, a seize or a restate, and nothing of a
    /// release, whose claim ref is gone. This machine's ref on the remote
    /// carries the entry that records the change, which is out of
    /// [`crate::ledger::appended`]: no send has it to carry.
    Landed(L),
    /// The remote applied nothing, and everything here is as before the
    /// change: this machine's ref holds what it held, and the entry is gone
    /// from disk and from [`crate::ledger::appended`].
    /// [`ClaimChanged::problems`] says what of that could not be put back.
    NotApplied {
        why: NotApplied,
        failure: LedgerFailure,
    },
    /// The remote may have applied the change or not: this machine's ref
    /// stays at the commit carrying the entry, which stays listed in
    /// [`crate::ledger::appended`], and a later send pushes it either way.
    Unknown { failure: LedgerFailure },
}

/// Why a claim's change applied nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotApplied {
    /// The claim was lost: git refused the push, and just after it the
    /// claim ref held `now` (`None`: no claim), not the claim commit the
    /// change expected. Another machine took, changed or released the claim
    /// while this ran.
    Lost { now: Option<CommitId> },
    /// git refused this machine's ref ([`Refusal::MachineRef`]): another
    /// machine may be committing under the same name.
    MachineRef,
    /// Anything else: git refused the push for another reason, or could not
    /// reach the remote; the remote has this machine's ref and this checkout
    /// never committed as the machine; or something here failed before
    /// anything was sent: a lock, the entry, its commit, or the claim
    /// commit.
    Failed,
}

/// Change a claim on a shared fork as `change` says, with the entry
/// `record` appends recording it.
///
/// Under the ledger's lock beside the root, then the destination's
/// transport lock, each waited for as long as a sweep waits, and never in
/// the other order:
///
/// 1. `record` appends the entry (with [`crate::ledger::Scribe::record_to`],
///    so it is listed in [`crate::ledger::appended`]) and returns its path;
/// 2. what is pending is committed to this machine's ref as a sweep commits
///    it ([`filtered_pending`]), moving it from P to a new commit N that
///    carries the entry;
/// 3. what the claim becomes is written ([`Becomes::write`]): for a take, a
///    seize or a restate, the claim commit, naming the entry's file name;
/// 4. the claim and this machine's ref go in one atomic push, leased on
///    `expected` ([`ledger_git::push_claim`]).
///
/// Where the push landed, the entry is on the remote, and it leaves
/// [`crate::ledger::appended`], so the send before the command exits has
/// nothing of it to look for. Where nothing was applied, this machine's ref
/// goes back to P and the entry goes ([`ClaimOutcome::NotApplied`]); where
/// the outcome is unknown, both stay ([`ClaimOutcome::Unknown`]). Where git
/// refused the push, whether the claim was lost is judged here, from the
/// claim ref as read just after it ([`NotApplied::Lost`]), and no caller
/// judges it again. Whatever the outcome, the repeated transitions the
/// commit discarded go to the sweep record, as a sweep's do, so `knives
/// status` still names them.
pub fn change_claim<B: Becomes>(
    change: ClaimChange<'_, B>,
    record: impl FnOnce() -> Result<PathBuf, LedgerError>,
) -> ClaimChanged<B::Landed> {
    let shared = change.shared;
    let root = shared.root.as_path();
    let unsent = |cause: String| ClaimChanged {
        outcome: ClaimOutcome::NotApplied {
            why: NotApplied::Failed,
            failure: LedgerFailure { cause },
        },
        problems: Vec::new(),
    };
    let locks = FileLock::acquire(root, LockWait::TRANSPORT).and_then(|ledger| {
        FileLock::acquire(shared.destination.transport_lock(), LockWait::TRANSPORT)
            .map(|transport| (ledger, transport))
    });
    let _locks = match locks {
        Ok(locks) => locks,
        Err(error) => return unsent(error.to_string()),
    };
    let path = match record() {
        Ok(path) => path,
        Err(error) => {
            return unsent(format!(
                "the entry recording the change was not written: {error}"
            ));
        }
    };
    let mut tally = Tally::of(&shared.destination);
    let mut problems = Vec::new();
    let outcome = match send(change, &path, &mut tally, &mut problems) {
        Sent::Landed(landed) => {
            crate::ledger::forget_appended(&path);
            ClaimOutcome::Landed(landed)
        }
        Sent::Unknown { cause } => ClaimOutcome::Unknown {
            failure: LedgerFailure { cause },
        },
        Sent::NotApplied { why, cause, moved } => {
            problems.extend(roll_back(&shared.destination, moved.as_ref(), &path));
            ClaimOutcome::NotApplied {
                why,
                failure: LedgerFailure { cause },
            }
        }
    };
    if let Err(error) = keep_discards(root, &mut tally) {
        problems.push(error.to_string());
    }
    ClaimChanged { outcome, problems }
}

/// This machine's ref before the commit of a change (P, `None` when it had
/// none) and after it (N).
type Moved = (Option<CommitId>, CommitId);

/// What [`send`] came to.
enum Sent<L> {
    /// Both refs moved, the claim ref to what the change made of it.
    Landed(L),
    /// Nothing was applied. `moved`: this machine's ref is still at N, to go
    /// back to P; `None` when it never moved, or the push moved it back.
    NotApplied {
        why: NotApplied,
        cause: String,
        moved: Option<Moved>,
    },
    Unknown {
        cause: String,
    },
}

/// Steps 2 to 4 of [`change_claim`], for the entry at `path`; the problems
/// the commit's check found go to `problems`, naming the destination.
fn send<B: Becomes>(
    change: ClaimChange<'_, B>,
    path: &Path,
    tally: &mut Tally,
    problems: &mut Vec<String>,
) -> Sent<B::Landed> {
    let ClaimChange {
        shared,
        claim,
        expected,
        becomes,
    } = change;
    let destination = &shared.destination;
    let repository = &destination.repository;
    let failed = |cause: String, moved: Option<Moved>| Sent::NotApplied {
        why: NotApplied::Failed,
        cause,
        moved,
    };
    let moved = match commit(destination, path, tally, problems) {
        Ok(moved) => moved,
        Err(cause) => return failed(cause, None),
    };
    let landed = match becomes.write(repository, path) {
        Ok(landed) => landed,
        Err(cause) => return failed(cause, Some(moved)),
    };
    let pushed = ledger_git::push_claim(
        repository,
        &ClaimPush {
            remote: REMOTE,
            machine: &destination.machine,
            claim,
            expected,
            new: B::commit(&landed),
            before: moved.0.as_ref(),
            after: &moved.1,
        },
    );
    match pushed {
        Ok(ClaimPushed::Landed) => Sent::Landed(landed),
        Ok(ClaimPushed::Refused {
            refusal,
            claim: seen,
            account,
        }) => Sent::NotApplied {
            why: why_refused(refusal, seen, expected),
            cause: account,
            moved: None,
        },
        Ok(ClaimPushed::Unknown { account }) => Sent::Unknown { cause: account },
        // The push may have reached the remote before this failed here (the
        // record of the remote's copy, or the move back of this machine's
        // ref), so nothing is put back.
        Err(error) => Sent::Unknown {
            cause: error.to_string(),
        },
    }
}

/// Why a push git refused applied nothing: the claim was lost where the
/// claim ref, as read just after it (`seen`), holds another commit than
/// `expected`, or none where the change expected one; else the ref git
/// refused.
fn why_refused(refusal: Refusal, seen: ClaimSeen, expected: Option<&CommitId>) -> NotApplied {
    match seen {
        ClaimSeen::At(now) if Some(&now) != expected => NotApplied::Lost { now: Some(now) },
        ClaimSeen::Absent if expected.is_some() => NotApplied::Lost { now: None },
        ClaimSeen::At(_) | ClaimSeen::Absent | ClaimSeen::Unread => match refusal {
            Refusal::MachineRef => NotApplied::MachineRef,
            Refusal::Other => NotApplied::Failed,
        },
    }
}

/// Step 2 of [`change_claim`]: commit what is pending through
/// `destination` as a sweep commits it, which must take in the entry at
/// `path`; this machine's ref before and after. The problems the commit's
/// check found go to `problems`, naming the destination. When this fails,
/// with what failed, nothing was sent and this machine's ref has not moved.
fn commit(
    destination: &Destination,
    path: &Path,
    tally: &mut Tally,
    problems: &mut Vec<String>,
) -> Result<Moved, String> {
    let repository = &destination.repository;
    let (pending, lines) =
        filtered_pending(destination, tally).map_err(|error| error.to_string())?;
    problems.extend(
        lines
            .into_iter()
            .map(|line| format!("{}: {line}", destination.describe())),
    );
    let committed = repository
        .relative(path)
        .is_some_and(|relative| pending.added().contains(&relative));
    if !committed {
        return Err(format!(
            "the entry {} is not one {} would commit",
            path.display(),
            destination.describe()
        ));
    }
    let before = pending.previous().cloned();
    match ledger_git::commit_pending(repository, &destination.machine, pending) {
        Ok(Some(after)) => Ok((before, after)),
        Ok(None) => Err("there was nothing to commit".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// Put back what a change that applied nothing left here: this machine's
/// ref from N to P, when `moved` says it is still at N, then the entry at
/// `path`, from disk and from [`crate::ledger::appended`]; what could not be
/// put back, each as a line. An entry whose commit could not be moved back
/// stays, since that commit carries it: a later send pushes both.
fn roll_back(destination: &Destination, moved: Option<&Moved>, path: &Path) -> Vec<String> {
    if let Some((before, after)) = moved
        && let Err(error) = ledger_git::restore_machine_ref(
            &destination.repository,
            &destination.machine,
            before.as_ref(),
            after,
        )
    {
        return vec![format!(
            "{}: {error}; the entry {} stays, and a later send carries it",
            destination.describe(),
            path.display()
        )];
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => vec![format!(
            "{}: could not remove the entry of a claim change that applied nothing: {error}; \
             a later send carries it",
            path.display()
        )],
        _ => {
            crate::ledger::forget_appended(path);
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::significant_drop_tightening,
        reason = "a lab holds the environment lock to the end of its test, past its last use"
    )]
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::ids::{BranchName, UpstreamName};
    use crate::ledger::{Draft, Ledger, Scribe};
    use crate::ledger_sweep::lab::{self, FORK, at, git};
    use crate::ledger_sweep::{ledger_of, shared_fork};
    use crate::store::OwnerKind;

    const MACHINE: &str = "refs/knives/machine-one";

    fn claim_of(branch: &str) -> ClaimRef {
        ClaimRef::new(&UpstreamName::new(FORK), &BranchName::new(branch)).unwrap()
    }

    fn record(branch: &str, machine: &str, owner: &str) -> ClaimRecord {
        ClaimRecord {
            repo: FORK.to_owned(),
            branch: branch.to_owned(),
            owner: owner.to_owned(),
            kind: OwnerKind::HarnessSession,
            why: "port the parser".to_owned(),
            started: "2026-11-16T08:00:00Z".to_owned(),
            place: Some("box-one 4026531836".to_owned()),
            machine: machine.to_owned(),
        }
    }

    /// Whether this process lists the entry at `path` as appended.
    fn listed(path: &Path) -> bool {
        crate::ledger::appended()
            .iter()
            .any(|appended| appended.path == path)
    }

    /// Every entry under `root` this process lists as appended.
    fn listed_under(root: &Path) -> Vec<PathBuf> {
        crate::ledger::appended()
            .into_iter()
            .map(|appended| appended.path)
            .filter(|path| path.starts_with(root))
            .collect()
    }

    /// The machine of [`lab::Lab`], sharing [`FORK`]'s ledger, and the
    /// destination that carries the fork there. It keeps the lab's hold on
    /// the environment lock until it drops.
    struct Lab {
        home: tempfile::TempDir,
        root: PathBuf,
        remote: PathBuf,
        shared: SharedFork,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Lab {
        fn new() -> Self {
            let lab::Lab {
                home,
                root,
                remote,
                registry,
                _lock: lock,
                ..
            } = lab::Lab::new();
            let fork = UpstreamName::new(FORK);
            let shared = shared_fork(
                &root,
                &registry,
                &fork,
                ledger_of(&registry, &fork).unwrap(),
            )
            .unwrap();
            Self {
                home,
                root,
                remote,
                shared,
                _lock: lock,
            }
        }

        fn git_dir(&self) -> &Path {
            self.shared.destination.repository.git_dir()
        }

        /// Change the claim of `claim` as a command does, with an entry of
        /// its own recording it; what came of it, and that entry.
        fn change<B: Becomes>(
            &self,
            claim: &ClaimRef,
            expected: Option<&CommitId>,
            becomes: B,
        ) -> (ClaimChanged<B::Landed>, PathBuf) {
            let scribe = Scribe::unanchored(
                Ledger::at(self.root.join(FORK)),
                UpstreamName::new(FORK),
                "owner-one".to_owned(),
            );
            let mut written = None;
            let changed = change_claim(
                ClaimChange {
                    shared: &self.shared,
                    claim,
                    expected,
                    becomes,
                },
                || {
                    let (_, path) = scribe.record_to(&Draft::event(
                        Some("feat/a"),
                        "a claim's change".into(),
                        None,
                    ))?;
                    written = Some(path.clone());
                    Ok(path)
                },
            );
            (changed, written.unwrap())
        }

        /// A take of `branch` by machine one that must land; its claim
        /// commit and entry.
        fn taken(&self, branch: &str) -> (CommitId, PathBuf) {
            let (changed, path) = self.change(
                &claim_of(branch),
                None,
                record(branch, "machine-one", "owner-one"),
            );
            match changed.outcome {
                ClaimOutcome::Landed(claim) => (claim, path),
                other => panic!("the take did not land: {other:?}"),
            }
        }

        /// `record`'s claim commit at `claim` on the remote, as another
        /// machine's landed push leaves it.
        fn held(&self, claim: &ClaimRef, record: &ClaimRecord) -> CommitId {
            let remote = Repository::new(
                &self.remote,
                &self.home.path().join("unused"),
                [UpstreamName::new(FORK)],
            )
            .unwrap();
            let commit =
                ledger_git::write_claim(&remote, record, "20261116T080000.000000002Z-c3d4.md")
                    .unwrap();
            git(
                &self.remote,
                &["update-ref", claim.as_str(), commit.as_str()],
            );
            commit
        }

        /// Whether the remote's ref of this machine carries the entry at `path`.
        fn sent(&self, path: &Path) -> bool {
            let relative = path.strip_prefix(&self.root).unwrap().to_str().unwrap();
            at(&self.remote, MACHINE).is_some_and(|commit| {
                git(
                    &self.remote,
                    &["ls-tree", "-r", "--name-only", commit.as_str()],
                )
                .lines()
                .any(|listed| listed == relative)
            })
        }
    }

    /// Why a change that must have applied nothing did not.
    fn why_not<L: std::fmt::Debug>(changed: ClaimChanged<L>) -> NotApplied {
        match changed.outcome {
            ClaimOutcome::NotApplied { why, .. } => why,
            other => panic!("applied, or unknown: {other:?}"),
        }
    }

    #[test]
    fn a_take_that_lands_puts_the_claim_and_its_entry_on_the_remote() {
        // Given: a machine sharing the fork's ledger, and no claim.
        let lab = Lab::new();
        let claim = claim_of("feat/a");

        // When: it takes the branch.
        let (changed, path) =
            lab.change(&claim, None, record("feat/a", "machine-one", "owner-one"));

        // Then: it landed, and the claim ref holds its claim commit.
        assert!(changed.problems.is_empty(), "{:?}", changed.problems);
        let ClaimOutcome::Landed(taken) = changed.outcome else {
            panic!("{:?}", changed.outcome);
        };
        assert_eq!(at(&lab.remote, claim.as_str()), Some(taken.clone()));
        // And: this machine's ref is on the remote, carrying the entry, which
        // is on disk here and no longer listed as this process's, since no
        // send has it to carry.
        assert_eq!(at(&lab.remote, MACHINE), at(lab.git_dir(), MACHINE));
        assert!(lab.sent(&path), "{}", path.display());
        assert!(path.is_file());
        assert!(!listed(&path));
        let entries = Ledger::at(lab.root.join(FORK)).read().unwrap().entries;
        let [entry] = entries.as_slice() else {
            panic!("{entries:?}");
        };
        assert_eq!(entry.text, "a claim's change");
        // And: the claim commit reads back as the record, and names that
        // entry.
        let held = read_claim(&lab.shared, &claim).unwrap().unwrap();
        assert_eq!(
            held,
            HeldClaim {
                commit: taken.clone(),
                record: Ok(record("feat/a", "machine-one", "owner-one"))
            }
        );
        let stored: toml::Table = toml::from_str(&git(
            &lab.remote,
            &["cat-file", "blob", &format!("{taken}:claim.toml")],
        ))
        .unwrap();
        assert_eq!(
            stored.get("entry").and_then(toml::Value::as_str),
            path.file_name().and_then(OsStr::to_str)
        );
    }

    #[test]
    fn a_take_that_loses_to_a_claim_already_there_puts_everything_back() {
        // Given: this machine's ref on the remote at P, and another machine's
        // claim of the branch.
        let lab = Lab::new();
        let (_, earlier) = lab.taken("feat/b");
        let before = at(lab.git_dir(), MACHINE).unwrap();
        let claim = claim_of("feat/a");
        let theirs = lab.held(&claim, &record("feat/a", "machine-two", "owner-two"));

        // When: this machine takes the branch, as though it read no claim.
        let (changed, _) = lab.change(&claim, None, record("feat/a", "machine-one", "owner-one"));

        // Then: the claim was lost to the claim there now.
        assert_eq!(
            why_not(changed),
            NotApplied::Lost {
                now: Some(theirs.clone())
            }
        );
        // And: the remote is as it was, this machine's ref is back at P, and
        // the entry is gone from disk and from the list, the earlier one
        // staying on disk.
        assert_eq!(at(&lab.remote, claim.as_str()), Some(theirs));
        assert_eq!(at(&lab.remote, MACHINE), Some(before.clone()));
        assert_eq!(at(lab.git_dir(), MACHINE), Some(before));
        let entries = Ledger::at(lab.root.join(FORK)).read().unwrap().entries;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(earlier.is_file());
        assert_eq!(listed_under(&lab.root), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_machine_ref_this_checkout_never_committed_is_refused_and_the_entry_goes() {
        // Given: the remote holding machine one's ref from another checkout,
        // which this one has fetched and never committed as.
        let lab = Lab::new();
        let tree = git(&lab.remote, &["mktree"]);
        let elsewhere = git(&lab.remote, &["commit-tree", "-m", "elsewhere", &tree]);
        git(&lab.remote, &["update-ref", MACHINE, &elsewhere]);
        ledger_git::fetch(&lab.shared.destination.repository, "origin").unwrap();
        let claim = claim_of("feat/a");

        // When: this checkout takes the branch.
        let (changed, _) = lab.change(&claim, None, record("feat/a", "machine-one", "owner-one"));

        // Then: it failed before anything was sent, saying the ref is not
        // its own.
        let ClaimOutcome::NotApplied {
            why: NotApplied::Failed,
            failure,
        } = changed.outcome
        else {
            panic!("{:?}", changed.outcome);
        };
        assert!(
            failure
                .cause
                .contains("this checkout never committed as it"),
            "{}",
            failure.cause
        );
        assert_eq!(at(&lab.remote, claim.as_str()), None);
        assert_eq!(at(&lab.remote, MACHINE).unwrap().as_str(), elsewhere);
        assert_eq!(at(lab.git_dir(), MACHINE), None);
        // And: the entry is gone from disk and from the list.
        assert_eq!(
            Ledger::at(lab.root.join(FORK)).read().unwrap().entries,
            Vec::new()
        );
        assert_eq!(listed_under(&lab.root), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_seize_leased_on_the_claim_it_read_lands() {
        // Given: another machine's claim, as this machine read it.
        let lab = Lab::new();
        let claim = claim_of("feat/a");
        let theirs = lab.held(&claim, &record("feat/a", "machine-two", "owner-two"));
        let read = read_claim(&lab.shared, &claim).unwrap().unwrap();
        assert_eq!(read.commit, theirs);

        // When: it seizes the claim, leasing on that commit.
        let (changed, path) = lab.change(
            &claim,
            Some(&read.commit),
            record("feat/a", "machine-one", "owner-one"),
        );

        // Then: the claim is this machine's, with its entry on the remote.
        let ClaimOutcome::Landed(mine) = changed.outcome else {
            panic!("{:?}", changed.outcome);
        };
        assert_ne!(mine, theirs);
        assert_eq!(at(&lab.remote, claim.as_str()), Some(mine));
        assert!(lab.sent(&path));
        let held = read_claim(&lab.shared, &claim).unwrap().unwrap();
        assert_eq!(held.record.unwrap().owner, "owner-one");
    }

    #[test]
    fn a_release_leased_on_the_claim_deletes_it_with_its_entry_sent() {
        // Given: this machine's landed claim.
        let lab = Lab::new();
        let claim = claim_of("feat/a");
        let (taken, _) = lab.taken("feat/a");

        // When: it releases the claim, leasing on its commit.
        let (changed, path) = lab.change(&claim, Some(&taken), Released);

        // Then: the claim ref is gone, and the release's entry was sent.
        assert!(
            matches!(changed.outcome, ClaimOutcome::Landed(())),
            "{:?}",
            changed.outcome
        );
        assert_eq!(at(&lab.remote, claim.as_str()), None);
        assert!(lab.sent(&path));
        assert_eq!(read_claim(&lab.shared, &claim).unwrap(), None);
    }

    #[test]
    fn a_release_whose_lease_is_stale_removes_nothing_and_puts_everything_back() {
        // Given: this machine's landed claim, which another machine has since
        // seized.
        let lab = Lab::new();
        let claim = claim_of("feat/a");
        let (taken, _) = lab.taken("feat/a");
        let before = at(lab.git_dir(), MACHINE).unwrap();
        let theirs = lab.held(&claim, &record("feat/a", "machine-two", "owner-two"));

        // When: this machine releases, leasing on its own commit.
        let (changed, _) = lab.change(&claim, Some(&taken), Released);

        // Then: the claim was lost to the claim there now.
        assert_eq!(
            why_not(changed),
            NotApplied::Lost {
                now: Some(theirs.clone())
            }
        );
        // And: nothing was removed on the remote, and here everything is back.
        assert_eq!(at(&lab.remote, claim.as_str()), Some(theirs));
        assert_eq!(at(&lab.remote, MACHINE), Some(before.clone()));
        assert_eq!(at(lab.git_dir(), MACHINE), Some(before));
        assert_eq!(
            Ledger::at(lab.root.join(FORK))
                .read()
                .unwrap()
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn a_seize_of_a_claim_released_meanwhile_is_lost_with_no_claim_there() {
        // Given: another machine's claim as this machine read it, which that
        // machine has since released.
        let lab = Lab::new();
        let claim = claim_of("feat/a");
        let theirs = lab.held(&claim, &record("feat/a", "machine-two", "owner-two"));
        let read = read_claim(&lab.shared, &claim).unwrap().unwrap();
        assert_eq!(read.commit, theirs);
        git(&lab.remote, &["update-ref", "-d", claim.as_str()]);

        // When: this machine seizes the claim, leasing on the commit it read.
        let (changed, path) = lab.change(
            &claim,
            Some(&read.commit),
            record("feat/a", "machine-one", "owner-one"),
        );

        // Then: the claim was lost, with no claim there now.
        assert_eq!(why_not(changed), NotApplied::Lost { now: None });
        // And: the remote holds no claim, and the entry is gone.
        assert_eq!(at(&lab.remote, claim.as_str()), None);
        assert!(!path.exists());
        assert_eq!(listed_under(&lab.root), Vec::<PathBuf>::new());
    }

    #[test]
    fn a_push_whose_outcome_is_unknown_keeps_the_commit_and_the_entry() {
        // Given: a remote whose receive-pack advertises its refs, takes the
        // commands and ends before it applies any.
        let lab = Lab::new();
        let script = lab.home.path().join("receive-pack");
        std::fs::write(
            &script,
            "#!/bin/sh\ngit receive-pack --advertise-refs \"$1\" || exit 1\n\
             head -c 4 >/dev/null\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            lab.git_dir(),
            &[
                "config",
                "remote.origin.receivepack",
                script.to_str().unwrap(),
            ],
        );
        let claim = claim_of("feat/a");

        // When: this machine takes the branch.
        let (changed, path) =
            lab.change(&claim, None, record("feat/a", "machine-one", "owner-one"));

        // Then: the outcome is unknown, and the remote holds nothing.
        assert!(
            matches!(changed.outcome, ClaimOutcome::Unknown { .. }),
            "{:?}",
            changed.outcome
        );
        assert_eq!(git(&lab.remote, &["for-each-ref"]), "");
        // And: this machine's ref stays at the commit carrying the entry,
        // which stays on disk and listed, for a later send.
        let kept = at(lab.git_dir(), MACHINE).unwrap();
        let relative = path.strip_prefix(&lab.root).unwrap().to_str().unwrap();
        assert!(
            git(
                lab.git_dir(),
                &["ls-tree", "-r", "--name-only", kept.as_str()]
            )
            .lines()
            .any(|listed| listed == relative)
        );
        assert!(path.is_file());
        assert!(listed(&path));
    }

    #[test]
    fn a_change_leaves_an_unreadable_entry_of_this_machine_out_and_says_so() {
        // Given: an entry file of this machine's that does not parse.
        let lab = Lab::new();
        let broken = lab
            .root
            .join(FORK)
            .join("20261116T080000.000000000Z-dead.md");
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, "not an entry\n").unwrap();

        // When: this machine takes a branch.
        let (changed, path) = lab.change(
            &claim_of("feat/a"),
            None,
            record("feat/a", "machine-one", "owner-one"),
        );

        // Then: the change said so, naming the destination, and went on: the
        // take's entry was sent and the broken one was not.
        assert!(
            changed.problems.iter().any(|problem| problem
                .starts_with(&format!("origin ({}): ", lab.git_dir().display()))
                && problem.contains("not sent until it is fixed or removed")),
            "{:?}",
            changed.problems
        );
        assert!(
            matches!(changed.outcome, ClaimOutcome::Landed(_)),
            "{:?}",
            changed.outcome
        );
        assert!(lab.sent(&path));
        assert!(!lab.sent(&broken));
        assert!(broken.is_file());
    }

    #[test]
    fn a_read_finds_no_claim_a_claim_or_a_claim_whose_record_does_not_read() {
        let lab = Lab::new();
        let claim = claim_of("feat/a");

        // No claim.
        assert_eq!(read_claim(&lab.shared, &claim).unwrap(), None);

        // A claim, with its record.
        let theirs = record("feat/a", "machine-two", "owner-two");
        let held = lab.held(&claim, &theirs);
        assert_eq!(
            read_claim(&lab.shared, &claim).unwrap(),
            Some(HeldClaim {
                commit: held,
                record: Ok(theirs)
            })
        );

        // A claim whose `claim.toml` is not a record: still a claim.
        let blob = {
            let path = lab.home.path().join("claim.toml");
            std::fs::write(&path, "owner = = ?\n").unwrap();
            git(&lab.remote, &["hash-object", "-w", path.to_str().unwrap()])
        };
        let listing = lab.home.path().join("listing");
        std::fs::write(&listing, format!("100644 blob {blob}\tclaim.toml\n")).unwrap();
        let tree = {
            let output = crate::bind::git_command()
                .arg("--git-dir")
                .arg(&lab.remote)
                .arg("mktree")
                .stdin(std::fs::File::open(&listing).unwrap())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let unreadable = git(&lab.remote, &["commit-tree", "-m", "claim", &tree]);
        git(&lab.remote, &["update-ref", claim.as_str(), &unreadable]);
        let read = read_claim(&lab.shared, &claim).unwrap().unwrap();
        assert_eq!(read.commit.as_str(), unreadable);
        assert!(read.record.is_err(), "{:?}", read.record);
    }

    #[test]
    fn a_read_that_fails_says_what_git_said() {
        // Given: a ledger remote that is gone.
        let lab = Lab::new();
        std::fs::remove_dir_all(&lab.remote).unwrap();

        // When: a claim is read.
        let failure = read_claim(&lab.shared, &claim_of("feat/a")).unwrap_err();

        // Then: it carries git's own account of the failure.
        assert!(
            failure
                .cause
                .contains("does not appear to be a git repository"),
            "{}",
            failure.cause
        );
    }
}
