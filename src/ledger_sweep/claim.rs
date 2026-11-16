//! A claim on a shared fork, read and changed through the destination that
//! carries the fork ([`shared_fork`](super::shared_fork)).
//!
//! [`crate::ledger_git`] reads a claim ref and pushes one atomic update; what
//! it leaves to this layer is the order around them. A change appends the
//! entry that records it, commits that entry to this machine's ref as a
//! sweep would ([`filtered_pending`]), and pushes the claim and this
//! machine's ref together. It holds the ledger's lock and then the
//! destination's transport lock, the order a sweep takes them in, so a
//! claim's change and a sweep never wait on each other. Where the push
//! applied nothing, everything here goes back as it was; where its outcome
//! is unknown, nothing does, since the remote may yet apply it and only the
//! new commit keeps this machine's ref a fast-forward of what it applies.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::kept::keep_discards;
use super::{Destination, REMOTE, SharedFork, SweepError, Tally, filtered_pending};
use crate::ids::CommitId;
use crate::ledger::{Entry, LedgerError};
use crate::ledger_git::{
    self, ClaimPush, ClaimPushed, ClaimRecord, ClaimRef, ClaimSeen, GitError, HeldClaim, Refusal,
};
use crate::lock::{FileLock, LockWait};

/// Why a claim could not be read or changed, and where: the ledger and the
/// git directory a command names with git's own text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the ledger {ledger} through {through}: {cause}")]
pub struct LedgerFailure {
    /// The `<owner>/<name>` of the ledger's repository.
    pub ledger: String,
    /// The destination, `origin (<git dir>)` ([`Destination::describe`]).
    pub through: String,
    /// What failed: git's own account, which quotes the remote, or what
    /// failed here. To show escaped.
    pub cause: String,
}

impl SharedFork {
    fn failure(&self, cause: String) -> LedgerFailure {
        LedgerFailure {
            ledger: self.ledger.clone(),
            through: self.destination.describe(),
            cause,
        }
    }
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
    let failed = |error: &dyn std::fmt::Display| shared.failure(error.to_string());
    let _transport = FileLock::acquire(shared.destination.transport_lock(), LockWait::TRANSPORT)
        .map_err(|error| failed(&error))?;
    ledger_git::read_claim(&shared.destination.repository, REMOTE, claim)
        .map_err(|error| failed(&error))
}

/// One change of a claim on a shared fork: a take, a seize, a restate or a
/// release.
#[derive(Debug)]
pub struct ClaimChange<'a> {
    /// The ledger root.
    pub root: &'a Path,
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
    /// this machine's ref on the remote carries the entry at `path`.
    Landed {
        claim: Option<CommitId>,
        entry: Box<Entry>,
        path: PathBuf,
    },
    /// The remote applied nothing, and everything here is as before the
    /// change: this machine's ref holds what it held, and the entry is gone
    /// from disk and from [`crate::ledger::appended`]. `problems` says what
    /// of that could not be put back.
    NotApplied {
        why: NotApplied,
        failure: LedgerFailure,
    },
    /// The remote may have applied the change or not: this machine's ref
    /// stays at the commit carrying the entry at `path`, which a later send
    /// pushes either way. `claim` is the claim ref as read just after.
    Unknown {
        claim: ClaimSeen,
        entry: Box<Entry>,
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
///    so it is listed in [`crate::ledger::appended`]) and returns it with
///    its path;
/// 2. what is pending is committed to this machine's ref as a sweep commits
///    it ([`filtered_pending`]), moving it from P to a new commit N that
///    carries the entry;
/// 3. a take, seize or restate writes the claim commit, its `entry` the
///    entry's file name;
/// 4. the claim and this machine's ref go in one atomic push, leased on
///    `expected` ([`ledger_git::push_claim`]).
///
/// Where nothing was applied, this machine's ref goes back to P and the
/// entry goes ([`ClaimOutcome::NotApplied`]); where the outcome is unknown,
/// both stay ([`ClaimOutcome::Unknown`]). Whatever the outcome, the repeated
/// transitions the commit discarded go to the sweep record, as a sweep's do,
/// so `knives status` still names them.
pub fn change_claim(
    change: ClaimChange<'_>,
    record: impl FnOnce() -> Result<(Entry, PathBuf), LedgerError>,
) -> ClaimChanged {
    let (root, shared) = (change.root, change.shared);
    let unsent = |cause: String| ClaimChanged {
        outcome: ClaimOutcome::NotApplied {
            why: NotApplied::Unsent,
            failure: shared.failure(cause),
        },
        problems: Vec::new(),
    };
    let locks =
        FileLock::acquire_noting_holder(root, LockWait::TRANSPORT).and_then(|(ledger, _)| {
            FileLock::acquire(shared.destination.transport_lock(), LockWait::TRANSPORT)
                .map(|transport| (ledger, transport))
        });
    let _locks = match locks {
        Ok(locks) => locks,
        Err(error) => return unsent(error.to_string()),
    };
    let (entry, path) = match record() {
        Ok(written) => written,
        Err(error) => {
            return unsent(format!(
                "the entry recording the change was not written: {error}"
            ));
        }
    };
    let mut tally = Tally::of(&shared.destination);
    let mut problems = Vec::new();
    let outcome = match send(change, &path, &mut tally, &mut problems) {
        Sent::Landed(claim) => ClaimOutcome::Landed {
            claim,
            entry: Box::new(entry),
            path,
        },
        Sent::Unknown { claim, cause } => ClaimOutcome::Unknown {
            claim,
            entry: Box::new(entry),
            path,
            failure: shared.failure(cause),
        },
        Sent::NotApplied { why, cause, moved } => {
            problems.extend(roll_back(&shared.destination, moved.as_ref(), &path));
            ClaimOutcome::NotApplied {
                why,
                failure: shared.failure(cause),
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
    let committed = std::path::absolute(path)
        .ok()
        .and_then(|path| {
            path.strip_prefix(repository.work_tree())
                .ok()
                .map(Path::to_path_buf)
        })
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
    use std::process::Stdio;

    use super::*;
    use crate::ids::{BranchName, UpstreamName};
    use crate::ledger::{Draft, Ledger, Scribe};
    use crate::ledger_git::Repository;
    use crate::ledger_sweep::{ledger_repositories, shared_fork};
    use crate::store::OwnerKind;

    const FORK: &str = "acme/a-repo";
    /// The ledger's URL as the git directory's `origin` spells it; git
    /// reaches it at the lab's bare remote through `insteadOf`.
    const LEDGER_URL: &str = "https://forge.invalid/acme/ledger";
    const MACHINE: &str = "refs/knives/machine-one";

    /// `git --git-dir <git_dir> <args>`, which must succeed; its stdout,
    /// trimmed.
    fn git(git_dir: &Path, args: &[&str]) -> String {
        let output = crate::bind::git_command()
            .arg("--git-dir")
            .arg(git_dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "lab")
            .env("GIT_AUTHOR_EMAIL", "lab@example.invalid")
            .env("GIT_COMMITTER_NAME", "lab")
            .env("GIT_COMMITTER_EMAIL", "lab@example.invalid")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn init_bare(git_dir: &Path) {
        let status = crate::bind::git_command()
            .args(["init", "--quiet", "--bare"])
            .arg(git_dir)
            .status()
            .unwrap();
        assert!(status.success(), "git init --bare {}", git_dir.display());
    }

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

    /// A machine sharing [`FORK`]'s ledger with acme/ledger through a git
    /// directory in `ledger-repositories`, as one set up by hand to share it
    /// has, committing as `machine-one`, and the bare remote git reaches as
    /// [`LEDGER_URL`].
    struct Lab {
        home: tempfile::TempDir,
        root: PathBuf,
        remote: PathBuf,
        shared: SharedFork,
    }

    impl Lab {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().join("ledger");
            let registry = home.path().join("repos.toml");
            std::fs::write(
                &registry,
                "[repos.a-repo]\nupstream = \"https://forge.invalid/acme/a-repo\"\n\
                 origin = \"https://forge.invalid/ours/a-repo\"\nledger = \"acme/ledger\"\n",
            )
            .unwrap();
            let registry = crate::config::load(&registry).unwrap();
            let remote = home.path().join("remote.git");
            init_bare(&remote);
            let git_dir = ledger_repositories(&root).join("acme--ledger.git");
            init_bare(&git_dir);
            let work_tree = root.to_str().unwrap();
            let instead_of = format!("url.{}.insteadOf", remote.display());
            for args in [
                ["config", "core.bare", "false"].as_slice(),
                &["config", "core.worktree", work_tree],
                &["remote", "add", "origin", LEDGER_URL],
                &["config", &instead_of, LEDGER_URL],
                &["config", "knives.machine", "machine-one"],
            ] {
                git(&git_dir, args);
            }
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
                    root: &self.root,
                    shared: &self.shared,
                    claim,
                    expected,
                    becomes,
                },
                || {
                    scribe.record_to(&Draft::event(
                        Some("feat/a"),
                        "a claim's change".into(),
                        None,
                    ))
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
    fn refused(changed: ClaimChanged) -> (Refusal, ClaimSeen, LedgerFailure) {
        match changed.outcome {
            ClaimOutcome::NotApplied {
                why: NotApplied::Refused { refusal, claim },
                failure,
            } => (refusal, claim, failure),
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
            entry,
        } = changed.outcome
        else {
            panic!("{:?}", changed.outcome);
        };
        assert_eq!(at(&lab.remote, claim.as_str()), Some(taken.clone()));
        // And: this machine's ref is on the remote, carrying the entry, which
        // is on disk here and listed as this process's.
        assert_eq!(at(&lab.remote, MACHINE), at(lab.git_dir(), MACHINE));
        assert!(lab.sent(&path), "{}", path.display());
        assert!(path.is_file());
        assert!(listed(&path));
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
        let (refusal, seen, failure) = refused(changed);
        assert_eq!(refusal, Refusal::Lease);
        assert_eq!(seen, ClaimSeen::At(theirs.clone()));
        assert_eq!(failure.ledger, "acme/ledger");
        assert_eq!(
            failure.through,
            format!("origin ({})", lab.git_dir().display())
        );
        // And: the remote is as it was, this machine's ref is back at P, and
        // the entry is gone from disk and from the list.
        assert_eq!(at(&lab.remote, claim.as_str()), Some(theirs));
        assert_eq!(at(&lab.remote, MACHINE), Some(before.clone()));
        assert_eq!(at(lab.git_dir(), MACHINE), Some(before));
        let entries = Ledger::at(lab.root.join(FORK)).read().unwrap().entries;
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(listed_under(&lab.root), [earlier]);
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
        let (refusal, seen, _) = refused(changed);
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
    fn a_read_that_fails_names_the_ledger_the_git_directory_and_gits_words() {
        // Given: a ledger remote that is gone.
        let lab = Lab::new();
        std::fs::remove_dir_all(&lab.remote).unwrap();

        // When: a claim is read.
        let failure = read_claim(&lab.shared, &claim_of("feat/a")).unwrap_err();

        // Then: it names the ledger, the git directory, and what git said.
        assert_eq!(failure.ledger, "acme/ledger");
        assert_eq!(
            failure.through,
            format!("origin ({})", lab.git_dir().display())
        );
        assert!(
            failure
                .cause
                .contains("does not appear to be a git repository"),
            "{}",
            failure.cause
        );
    }
}
