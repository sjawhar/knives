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
use super::{Destination, REMOTE, SharedFork, SweepError, Tally, filtered_pending};
use crate::ids::CommitId;
use crate::ledger::LedgerError;
use crate::ledger_git::{
    self, ClaimPush, ClaimPushed, ClaimRecord, ClaimRef, ClaimSeen, GitError, HeldClaim, Refusal,
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

/// One change of a claim on a shared fork: a take, a seize, a restate or a
/// release.
#[derive(Debug)]
pub struct ClaimChange<'a> {
    pub shared: &'a SharedFork,
    pub claim: &'a ClaimRef,
    /// The claim commit the remote must hold for the change to land, as the
    /// caller read it; `None` for a take, which lands only on no claim.
    pub expected: Option<&'a CommitId>,
    /// What the claim becomes, its `entry` set here to the file name of the
    /// entry that records the change; `None` releases it.
    pub becomes: Option<ClaimRecord>,
}

/// What came of a [`change_claim`].
#[derive(Debug)]
pub struct ClaimChanged {
    pub outcome: ClaimOutcome,
    /// What the commit's check passed over ([`filtered_pending`]), each line
    /// naming the destination, and what could not be put back or kept: to
    /// print as a sweep prints its problems. None of them stopped the change.
    pub problems: Vec<String>,
}

/// Whether a claim's change reached the ledger's remote.
#[derive(Debug)]
pub enum ClaimOutcome {
    /// The claim ref holds `claim`, or is gone when `claim` is `None`, and
    /// this machine's ref on the remote carries the entry at `path`, which
    /// is out of [`crate::ledger::appended`]: no send has it to carry.
    Landed {
        claim: Option<CommitId>,
        path: PathBuf,
    },
    /// The remote applied nothing, and everything here is as before the
    /// change: this machine's ref holds what it held, and the entry is gone
    /// from disk and from [`crate::ledger::appended`].
    /// [`ClaimChanged::problems`] says what of that could not be put back.
    NotApplied {
        why: NotApplied,
        failure: LedgerFailure,
    },
    /// The remote may have applied the change or not: this machine's ref
    /// stays at the commit carrying the entry at `path`, which a later send
    /// pushes either way. `claim` is the claim ref as read just after.
    Unknown {
        claim: ClaimSeen,
        path: PathBuf,
        failure: LedgerFailure,
    },
}

/// Why a claim's change applied nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotApplied {
    /// git refused the push ([`Refusal`]). `claim` is what the remote's claim
    /// ref held just after: another commit than the lease expected means the
    /// claim was taken or changed while this ran.
    Refused { refusal: Refusal, claim: ClaimSeen },
    /// The remote has this machine's ref and this checkout never committed
    /// as the machine ([`GitError::NotOwn`]): nothing was committed or sent.
    NotOwn,
    /// Something here failed before anything was sent: a lock, the entry,
    /// its commit, or the claim commit.
    Unsent,
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
/// 3. a take, seize or restate writes the claim commit, its `entry` the
///    entry's file name;
/// 4. the claim and this machine's ref go in one atomic push, leased on
///    `expected` ([`ledger_git::push_claim`]).
///
/// Where the push landed, the entry is on the remote, and it leaves
/// [`crate::ledger::appended`], so the send before the command exits has
/// nothing of it to look for. Where nothing was applied, this machine's ref
/// goes back to P and the entry goes ([`ClaimOutcome::NotApplied`]); where
/// the outcome is unknown, both stay ([`ClaimOutcome::Unknown`]). Whatever
/// the outcome, the repeated transitions the commit discarded go to the
/// sweep record, as a sweep's do, so `knives status` still names them.
pub fn change_claim(
    change: ClaimChange<'_>,
    record: impl FnOnce() -> Result<PathBuf, LedgerError>,
) -> ClaimChanged {
    let shared = change.shared;
    let root = shared.root.as_path();
    let unsent = |cause: String| ClaimChanged {
        outcome: ClaimOutcome::NotApplied {
            why: NotApplied::Unsent,
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
        Sent::Landed(claim) => {
            crate::ledger::forget_appended(&path);
            ClaimOutcome::Landed { claim, path }
        }
        Sent::Unknown { claim, cause } => ClaimOutcome::Unknown {
            claim,
            path,
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
enum Sent {
    /// Both refs moved; the claim ref holds this commit, or is gone.
    Landed(Option<CommitId>),
    /// Nothing was applied. `moved`: this machine's ref is still at N, to go
    /// back to P; `None` when it never moved, or the push moved it back.
    NotApplied {
        why: NotApplied,
        cause: String,
        moved: Option<Moved>,
    },
    Unknown {
        claim: ClaimSeen,
        cause: String,
    },
}

/// Steps 2 to 4 of [`change_claim`], for the entry at `path`; the problems
/// the commit's check found go to `problems`, naming the destination.
fn send(
    change: ClaimChange<'_>,
    path: &Path,
    tally: &mut Tally,
    problems: &mut Vec<String>,
) -> Sent {
    let destination = &change.shared.destination;
    let repository = &destination.repository;
    let moved = match commit(destination, path, tally, problems) {
        Ok(moved) => moved,
        Err((why, cause)) => {
            return Sent::NotApplied {
                why,
                cause,
                moved: None,
            };
        }
    };
    let written = change
        .becomes
        .map(|mut record| {
            path.file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .clone_into(&mut record.entry);
            ledger_git::write_claim(repository, &record)
        })
        .transpose();
    let new = match written {
        Ok(new) => new,
        Err(error) => {
            return Sent::NotApplied {
                why: NotApplied::Unsent,
                cause: error.to_string(),
                moved: Some(moved),
            };
        }
    };
    let pushed = ledger_git::push_claim(
        repository,
        &ClaimPush {
            remote: REMOTE,
            machine: &destination.machine,
            claim: change.claim,
            expected: change.expected,
            new: new.as_ref(),
            before: moved.0.as_ref(),
            after: &moved.1,
        },
    );
    match pushed {
        Ok(ClaimPushed::Landed) => Sent::Landed(new),
        Ok(ClaimPushed::Refused {
            refusal,
            claim,
            account,
        }) => Sent::NotApplied {
            why: NotApplied::Refused { refusal, claim },
            cause: account,
            moved: None,
        },
        Ok(ClaimPushed::Unknown { claim, account }) => Sent::Unknown {
            claim,
            cause: account,
        },
        // The push may have reached the remote before this failed here (the
        // record of the remote's copy, or the move back of this machine's
        // ref), so nothing is put back.
        Err(error) => Sent::Unknown {
            claim: ClaimSeen::Unread(error.to_string()),
            cause: error.to_string(),
        },
    }
}

/// Step 2 of [`change_claim`]: commit what is pending through
/// `destination` as a sweep commits it, which must take in the entry at
/// `path`; this machine's ref before and after. The problems the commit's
/// check found go to `problems`, naming the destination. When this fails,
/// with why and what failed, nothing was sent and this machine's ref has not
/// moved.
fn commit(
    destination: &Destination,
    path: &Path,
    tally: &mut Tally,
    problems: &mut Vec<String>,
) -> Result<Moved, (NotApplied, String)> {
    let repository = &destination.repository;
    let unsent = |cause: String| (NotApplied::Unsent, cause);
    let (pending, lines) = match filtered_pending(destination, tally) {
        Ok(filtered) => filtered,
        Err(SweepError::Git(error @ GitError::NotOwn { .. })) => {
            return Err((NotApplied::NotOwn, error.to_string()));
        }
        Err(error) => return Err(unsent(error.to_string())),
    };
    problems.extend(
        lines
            .into_iter()
            .map(|line| format!("{}: {line}", destination.describe())),
    );
    let committed = repository
        .relative(path)
        .is_some_and(|relative| pending.added().contains(&relative));
    if !committed {
        return Err(unsent(format!(
            "the entry {} is not one {} would commit",
            path.display(),
            destination.describe()
        )));
    }
    let before = pending.previous().cloned();
    match ledger_git::commit_pending(repository, &destination.machine, pending) {
        Ok(Some(after)) => Ok((before, after)),
        Ok(None) => Err(unsent("there was nothing to commit".to_owned())),
        Err(error) => Err(unsent(error.to_string())),
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
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::ids::{BranchName, UpstreamName};
    use crate::ledger::{Draft, Ledger, Scribe};
    use crate::ledger_git::Repository;
    use crate::ledger_sweep::lab::{self, FORK, git};
    use crate::ledger_sweep::shared_fork;
    use crate::store::OwnerKind;

    const MACHINE: &str = "refs/knives/machine-one";

    /// The commit `reference` holds in `git_dir`, if it exists.
    fn at(git_dir: &Path, reference: &str) -> Option<CommitId> {
        let id = git(
            git_dir,
            &["for-each-ref", "--format=%(objectname)", reference],
        );
        (!id.is_empty()).then(|| CommitId::new(id))
    }

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
            entry: String::new(),
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
    /// destination that carries the fork there.
    struct Lab {
        home: tempfile::TempDir,
        root: PathBuf,
        remote: PathBuf,
        shared: SharedFork,
    }

    impl Lab {
        fn new() -> Self {
            let lab::Lab {
                home,
                root,
                remote,
                registry,
                ..
            } = lab::Lab::new();
            let shared = shared_fork(&root, &registry, &UpstreamName::new(FORK))
                .unwrap()
                .unwrap();
            Self {
                home,
                root,
                remote,
                shared,
            }
        }

        fn git_dir(&self) -> &Path {
            self.shared.destination.repository.git_dir()
        }

        /// Change the claim of `claim` as a command does, with an entry of
        /// its own recording it.
        fn change(
            &self,
            claim: &ClaimRef,
            expected: Option<&CommitId>,
            becomes: Option<ClaimRecord>,
        ) -> ClaimChanged {
            let scribe = Scribe::unanchored(
                Ledger::at(self.root.join(FORK)),
                UpstreamName::new(FORK),
                "owner-one".to_owned(),
            );
            change_claim(
                ClaimChange {
                    shared: &self.shared,
                    claim,
                    expected,
                    becomes,
                },
                || {
                    scribe
                        .record_to(&Draft::event(
                            Some("feat/a"),
                            "a claim's change".into(),
                            None,
                        ))
                        .map(|(_, path)| path)
                },
            )
        }

        /// A take of `branch` by machine one that must land; its claim
        /// commit and entry.
        fn taken(&self, branch: &str) -> (CommitId, PathBuf) {
            let changed = self.change(
                &claim_of(branch),
                None,
                Some(record(branch, "machine-one", "owner-one")),
            );
            match changed.outcome {
                ClaimOutcome::Landed {
                    claim: Some(claim),
                    path,
                    ..
                } => (claim, path),
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
            let commit = ledger_git::write_claim(&remote, record).unwrap();
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

    /// The why and the claim seen of a change that must have been refused.
    fn refused(changed: ClaimChanged) -> (Refusal, ClaimSeen) {
        match changed.outcome {
            ClaimOutcome::NotApplied {
                why: NotApplied::Refused { refusal, claim },
                ..
            } => (refusal, claim),
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn a_take_that_lands_puts_the_claim_and_its_entry_on_the_remote() {
        // Given: a machine sharing the fork's ledger, and no claim.
        let lab = Lab::new();
        let claim = claim_of("feat/a");

        // When: it takes the branch.
        let changed = lab.change(
            &claim,
            None,
            Some(record("feat/a", "machine-one", "owner-one")),
        );

        // Then: it landed, and the claim ref holds its claim commit.
        assert!(changed.problems.is_empty(), "{:?}", changed.problems);
        let ClaimOutcome::Landed {
            claim: Some(taken),
            path,
        } = changed.outcome
        else {
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
        // And: the claim commit names that entry, and reads back.
        let held = read_claim(&lab.shared, &claim).unwrap().unwrap();
        assert_eq!(held.commit, taken);
        assert_eq!(
            held.record.unwrap().entry,
            path.file_name().unwrap().to_str().unwrap()
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
        let changed = lab.change(
            &claim,
            None,
            Some(record("feat/a", "machine-one", "owner-one")),
        );

        // Then: it was refused on the lease, and names the claim there now.
        let (refusal, seen) = refused(changed);
        assert_eq!(refusal, Refusal::Lease);
        assert_eq!(seen, ClaimSeen::At(theirs.clone()));
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
        let changed = lab.change(
            &claim,
            None,
            Some(record("feat/a", "machine-one", "owner-one")),
        );

        // Then: it is refused as not its own, before anything was sent.
        let ClaimOutcome::NotApplied {
            why: NotApplied::NotOwn,
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
        let changed = lab.change(
            &claim,
            Some(&read.commit),
            Some(record("feat/a", "machine-one", "owner-one")),
        );

        // Then: the claim is this machine's, with its entry on the remote.
        let ClaimOutcome::Landed {
            claim: Some(mine),
            path,
            ..
        } = changed.outcome
        else {
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
        let changed = lab.change(&claim, Some(&taken), None);

        // Then: the claim ref is gone, and the release's entry was sent.
        let ClaimOutcome::Landed {
            claim: None, path, ..
        } = changed.outcome
        else {
            panic!("{:?}", changed.outcome);
        };
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
        let changed = lab.change(&claim, Some(&taken), None);

        // Then: the release is refused on the lease, naming the claim there.
        let (refusal, seen) = refused(changed);
        assert_eq!(refusal, Refusal::Lease);
        assert_eq!(seen, ClaimSeen::At(theirs.clone()));
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
        let changed = lab.change(
            &claim,
            None,
            Some(record("feat/a", "machine-one", "owner-one")),
        );

        // Then: the outcome is unknown, and the remote holds nothing.
        let ClaimOutcome::Unknown {
            claim: seen, path, ..
        } = changed.outcome
        else {
            panic!("{:?}", changed.outcome);
        };
        assert_eq!(seen, ClaimSeen::Absent);
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
        let changed = lab.change(
            &claim_of("feat/a"),
            None,
            Some(record("feat/a", "machine-one", "owner-one")),
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
        let ClaimOutcome::Landed { path, .. } = changed.outcome else {
            panic!("{:?}", changed.outcome);
        };
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
