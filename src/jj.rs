use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use jj_lib::backend::CommitId as JjCommitId;
use jj_lib::config::StackedConfig;
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoader, StoreFactories};
use jj_lib::revset::SymbolResolver;
use jj_lib::settings::UserSettings;
use jj_lib::working_copy::WorkingCopy as _;
use thiserror::Error;

use crate::detect::landed::RebaseOutcome;
use crate::detect::stale_parents::{BookmarkTips, ReleaseParent};
use crate::ids::{
    BookmarkRef, BranchName, ChangeId, CommitId, ReleaseScheme, RemoteName, WorkspaceName,
    is_our_release, pull_number_from_bookmark,
};

#[derive(Debug, Error)]
pub enum JjError {
    #[error("could not open jj repository at {path}: {detail}")]
    Open { path: String, detail: String },
    #[error("could not resolve revision `{revision}`: {detail}")]
    Revision { revision: String, detail: String },
    #[error("reference `{name}` is absent or conflicted")]
    RefTarget { name: String },
    #[error("command `{program}` failed ({status}): {stderr}")]
    Command {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("could not run `{program}`: {detail}")]
    Process { program: String, detail: String },
    #[error("probe did not create exactly one root commit")]
    ProbeRoot,
    #[error("could not parse command output: {detail}")]
    Parse { detail: String },
}

/// Twelve characters is what jj shows, and a full id is correct and unreadable.
fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

#[derive(Debug)]
pub struct Repo {
    repo: Arc<ReadonlyRepo>,
}

impl Repo {
    /// Whether this workspace's working copy is behind the repository.
    ///
    /// jj refuses to run in a stale workspace, but this tool reads through jj-lib, so
    /// it answered happily while `jj` itself errored. The detectors replay commits
    /// onto the trunk, so a working copy that does not match the repository can
    /// invalidate their conclusions with nothing said. Observed on a managed checkout,
    /// which reported normally while every `jj` command in it failed.
    ///
    /// Reads the recorded checkout state only. It does not snapshot, which matters:
    /// these repositories are worked concurrently, and snapshotting another agent's
    /// working copy would be a mutation.
    pub fn stale_working_copy(&self, path: &Path) -> Option<String> {
        let settings = UserSettings::from_config(StackedConfig::with_defaults()).ok()?;
        let working_copy = LocalWorkingCopy::load(
            self.repo.store().clone(),
            path.to_owned(),
            path.join(".jj/working_copy"),
            &settings,
        )
        .ok()?;
        let recorded = working_copy.operation_id();
        let current = self.repo.operation().id();
        (recorded != current).then(|| {
            format!(
                "working copy is stale (recorded at operation {}, repository is at {}); \
                 run `jj workspace update-stale` in {}",
                short_id(&recorded.hex()),
                short_id(&current.hex()),
                path.display()
            )
        })
    }

    pub fn open(path: &Path) -> Result<Self, JjError> {
        let settings =
            UserSettings::from_config(StackedConfig::with_defaults()).map_err(|error| {
                JjError::Open {
                    path: path.display().to_string(),
                    detail: error.to_string(),
                }
            })?;
        let loader = RepoLoader::init_from_file_system(
            &settings,
            &path.join(".jj/repo"),
            &StoreFactories::default(),
        )
        .map_err(|error| JjError::Open {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
        let repo = block_on(loader.load_at_head()).map_err(|error| JjError::Open {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
        Ok(Self { repo })
    }

    pub fn workspaces(&self) -> Result<Vec<(WorkspaceName, ChangeId)>, JjError> {
        self.repo
            .view()
            .wc_commit_ids()
            .iter()
            .map(|(name, commit)| {
                let commit =
                    self.repo
                        .store()
                        .get_commit(commit)
                        .map_err(|error| JjError::Open {
                            path: "commit store".to_owned(),
                            detail: error.to_string(),
                        })?;
                Ok((
                    WorkspaceName::new(name.as_symbol().to_string()),
                    ChangeId::new(commit.change_id().to_string()),
                ))
            })
            .collect()
    }

    pub fn bookmark_tips(&self) -> Result<BookmarkTips, JjError> {
        let mut tips = BTreeMap::new();
        for (branch, targets) in self.repo.view().bookmarks() {
            let branch = BranchName::new(branch.as_str());
            if let Some(commit) = targets.local_target.as_normal() {
                tips.insert(BookmarkRef::Local(branch.clone()), commit_id(commit));
            }
            for (remote, target) in targets.remote_refs {
                if let Some(commit) = target.target.as_normal() {
                    tips.insert(
                        BookmarkRef::Remote {
                            branch: branch.clone(),
                            remote: RemoteName::new(remote.as_str()),
                        },
                        commit_id(commit),
                    );
                }
            }
        }
        Ok(tips)
    }

    /// Bookmarks whose target is conflicted, with every commit they point at.
    ///
    /// These are exactly the divergent branches, and `bookmark_tips` cannot
    /// carry them: a conflicted ref has no single commit, so `as_normal` is
    /// None and it drops out of the tip map. Dropping them silently is the
    /// opposite of what is wanted, because divergence is routine and the
    /// observed failure is an agent seeing the markers, reading corruption, and
    /// stopping. A branch that is divergent must still appear, with its cause.
    pub fn conflicted_bookmarks(&self) -> Result<Vec<(BookmarkRef, Vec<CommitId>)>, JjError> {
        let mut found = Vec::new();
        for (branch, targets) in self.repo.view().bookmarks() {
            let name = BranchName::new(branch.as_str());
            if targets.local_target.as_normal().is_none()
                && targets.local_target.added_ids().next().is_some()
            {
                found.push((
                    BookmarkRef::Local(name.clone()),
                    targets.local_target.added_ids().map(commit_id).collect(),
                ));
            }
            for (remote, target) in targets.remote_refs {
                if target.target.as_normal().is_none() && target.target.added_ids().next().is_some()
                {
                    found.push((
                        BookmarkRef::Remote {
                            branch: name.clone(),
                            remote: RemoteName::new(remote.as_str()),
                        },
                        target.target.added_ids().map(commit_id).collect(),
                    ));
                }
            }
        }
        Ok(found)
    }

    pub fn parents_of(&self, revision: &str) -> Result<Vec<ReleaseParent>, JjError> {
        let commit = self.commit(revision)?;
        commit
            .parent_ids()
            .iter()
            .map(|parent| {
                let bookmarks = self
                    .bookmark_tips()?
                    .into_iter()
                    .filter_map(|(bookmark, tip)| {
                        (tip.as_str() == parent.to_string()).then_some(bookmark)
                    })
                    .collect();
                Ok(ReleaseParent {
                    commit: commit_id(parent),
                    bookmarks,
                })
            })
            .collect()
    }

    pub fn divergent_changes(&self) -> Result<Vec<(ChangeId, CommitId)>, JjError> {
        let mut changes = BTreeMap::<ChangeId, BTreeSet<CommitId>>::new();
        for head in self.repo.view().heads() {
            let commit = self
                .repo
                .store()
                .get_commit(head)
                .map_err(|error| JjError::Open {
                    path: "commit store".to_owned(),
                    detail: error.to_string(),
                })?;
            let change = ChangeId::new(commit.change_id().to_string());
            if let Some(targets) =
                self.repo
                    .resolve_change_id(commit.change_id())
                    .map_err(|error| JjError::Open {
                        path: "change index".to_owned(),
                        detail: error.to_string(),
                    })?
            {
                let commits = changes.entry(change).or_default();
                for (_, id) in targets.visible_with_offsets() {
                    commits.insert(commit_id(id));
                }
            }
        }
        Ok(changes
            .into_iter()
            .filter(|(_, commits)| commits.len() > 1)
            .flat_map(|(change, commits)| {
                commits
                    .into_iter()
                    .map(move |commit| (change.clone(), commit))
            })
            .collect())
    }

    /// Whether `ancestor` is reachable from `descendant`.
    pub fn is_ancestor(&self, ancestor: &CommitId, descendant: &CommitId) -> Result<bool, JjError> {
        let ancestor = self.commit(ancestor.as_str())?;
        let descendant = self.commit(descendant.as_str())?;
        self.repo
            .index()
            .is_ancestor(ancestor.id(), descendant.id())
            .map_err(|error| JjError::Revision {
                revision: descendant.id().to_string(),
                detail: error.to_string(),
            })
    }

    /// Bookmarks whose history includes `commit`, excluding any pointing exactly at it.
    ///
    /// Answers where work went when it was not merged: a maintainer building their own
    /// branch on our commits leaves the branch itself untouched, so the only trace is that
    /// its tip is reachable from somewhere else.
    pub fn branches_containing(
        &self,
        commit: &CommitId,
        scheme: &ReleaseScheme,
    ) -> Result<Vec<BookmarkRef>, JjError> {
        let mut found = Vec::new();
        for (reference, tip) in self.bookmark_tips()? {
            if &tip == commit {
                continue;
            }
            // Our own releases carry our own branches BY CONSTRUCTION — a cut is a flat
            // octopus merge of these very tips — so every carried branch is trivially
            // reachable from every release containing it. Reporting that says nothing the
            // reader does not already know, and it buries the case this check exists for.
            // Measured on a real repository before this filter: 10 findings, every carrier
            // a release or a `@git` ref, zero true positives.
            //
            // `@git` is jj's internal git-tracking view rather than a remote, and is
            // excluded everywhere else in this codebase for the same reason.
            // A fetched head is our own pull request, not someone else carrying the work.
            if is_our_release(&reference, scheme)
                || matches!(&reference, BookmarkRef::Remote { remote, .. } if remote.as_str() == "git")
                || pull_number_from_bookmark(reference.branch().as_str()).is_some()
            {
                continue;
            }
            if self.is_ancestor(commit, &tip)? {
                found.push(reference);
            }
        }
        Ok(found)
    }

    pub fn resolve_commit(&self, revision: &str) -> Result<CommitId, JjError> {
        Ok(commit_id(&self.commit(revision)?.id().clone()))
    }

    fn commit(&self, revision: &str) -> Result<jj_lib::commit::Commit, JjError> {
        // `name@remote` is a revset construct, not a symbol: jj-lib's resolver knows
        // change ids and local bookmarks, so asking it for `main@upstream` fails even
        // though the jj CLI resolves it. The remote ref is in the view, so read it there.
        if let Some((branch, remote)) = revision.split_once('@') {
            let wanted = BookmarkRef::Remote {
                branch: BranchName::new(branch),
                remote: RemoteName::new(remote),
            };
            if let Some(commit) = self.bookmark_tips()?.get(&wanted) {
                return self
                    .repo
                    .store()
                    .get_commit(&JjCommitId::try_from_hex(commit.as_str()).ok_or_else(|| {
                        JjError::Revision {
                            revision: revision.to_owned(),
                            detail: "remote ref is not a hex commit id".to_owned(),
                        }
                    })?)
                    .map_err(|error| JjError::Revision {
                        revision: revision.to_owned(),
                        detail: error.to_string(),
                    });
            }
        }
        let extensions: [Box<dyn jj_lib::revset::SymbolResolverExtension>; 0] = [];
        let id = SymbolResolver::new(self.repo.as_ref(), &extensions)
            .resolve_symbol(self.repo.as_ref(), revision)
            .map_err(|error| JjError::Revision {
                revision: revision.to_owned(),
                detail: error.to_string(),
            })?;
        self.repo
            .store()
            .get_commit(&id)
            .map_err(|error| JjError::Revision {
                revision: revision.to_owned(),
                detail: error.to_string(),
            })
    }
}

/// Fetches every configured remote through jj porcelain because fetch updates git-backed remote state.
pub fn fetch_all(repo: &Path) -> Result<(), JjError> {
    let repo = path(repo);
    // `--ignore-working-copy` is not an optimisation here, it is what makes the
    // command usable at all in a shared repository. Without it, jj refuses with
    // "the working copy is stale" whenever another agent has moved the repo on
    // since this workspace last looked, which is the normal state of affairs
    // when several agents share a checkout. Fetching updates refs; it has no
    // business touching, or being blocked by, someone else's working copy.
    command(
        "jj",
        [
            "--repository",
            &repo,
            "--ignore-working-copy",
            "git",
            "fetch",
            "--all-remotes",
        ],
    )?;
    Ok(())
}

/// Reads pull refs directly from the git transport because jj-lib intentionally has no forge-ref API.
pub fn pull_heads(_repo: &Path, remote_url: &str) -> Result<BTreeMap<u64, String>, JjError> {
    let output = command("git", ["ls-remote", remote_url, "refs/pull/*/head"])?;
    output.lines().try_fold(BTreeMap::new(), |mut heads, line| {
        let (sha, reference) = line.split_once('\t').ok_or_else(|| JjError::Parse {
            detail: line.to_owned(),
        })?;
        let number = reference
            .strip_prefix("refs/pull/")
            .and_then(|value| value.strip_suffix("/head"))
            .ok_or_else(|| JjError::Parse {
                detail: line.to_owned(),
            })?
            .parse::<u64>()
            .map_err(|error| JjError::Parse {
                detail: error.to_string(),
            })?;
        heads.insert(number, sha.to_owned());
        Ok(heads)
    })
}

/// Uses jj porcelain because replaying commits and materializing conflicts is not a jj-lib read operation.
pub fn probe_landed(
    repo: &Path,
    branch: &BranchName,
    onto: &str,
) -> Result<RebaseOutcome, JjError> {
    let repo_path = path(repo);

    // `--ignore-working-copy` on the duplicate too. Without it the command
    // snapshots, which rewrites a dirty `@`'s commit id and, in the old set
    // difference cleanup, made another agent's working commit look like ours.
    let range = format!("{onto}..{branch}");
    let (_, reported) = command_output(
        "jj",
        &[
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "duplicate",
            "-r",
            &range,
            "--onto",
            onto,
        ],
    )?;

    let created = parse_duplicated(&reported);
    // Armed the instant anything could exist, holding ids rather than a query.
    let cleanup = ProbeCleanup {
        repo,
        created: created.clone(),
    };

    if created.is_empty() {
        // Nothing to duplicate: the branch holds no commit that `onto` lacks, so
        // its content is already there. A merge without squashing lands here.
        drop(cleanup);
        return Ok(RebaseOutcome::Empty);
    }

    // Every duplicated commit, not just the first. A branch of several commits
    // duplicates as several, and judging the branch by one of them answers a
    // different question.
    let revset = created
        .iter()
        .map(|commit| format!("descendants({})", commit.as_str()))
        .collect::<Vec<_>>()
        .join("|");

    let state = command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "empty ++ \"\\t\" ++ conflict ++ \"\\n\"",
        ],
    )?;
    let rows: Vec<(bool, bool)> = state
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            (parts.next() == Some("true"), parts.next() == Some("true"))
        })
        .collect();
    drop(cleanup);

    if rows.is_empty() {
        return Err(JjError::ProbeRoot);
    }
    if rows.iter().any(|(_, conflicted)| *conflicted) {
        return Ok(RebaseOutcome::Conflicted);
    }
    if rows.iter().all(|(empty, _)| *empty) {
        return Ok(RebaseOutcome::Empty);
    }
    Ok(RebaseOutcome::CleanNonEmpty)
}

/// Uses jj porcelain because workspace creation updates jj's workspace metadata and filesystem layout.
pub fn add_workspace(
    repo: &Path,
    name: &str,
    destination: &Path,
    revision: &str,
) -> Result<(), JjError> {
    let repo = path(repo);
    let destination = path(destination);
    command(
        "jj",
        [
            "--repository",
            &repo,
            "workspace",
            "add",
            "--name",
            name,
            "-r",
            revision,
            &destination,
        ],
    )?;
    Ok(())
}

/// Reads git's remote configuration because jj-lib does not expose remote URLs as a typed repository view.
pub fn git_toplevel(repo: &Path) -> Result<PathBuf, JjError> {
    let repo = path(repo);
    let output = command("git", ["-C", &repo, "rev-parse", "--show-toplevel"])?;
    Ok(PathBuf::from(output.trim()))
}

pub fn git_remotes(repo: &Path) -> Result<BTreeMap<String, String>, JjError> {
    let repo = path(repo);
    let output = command(
        "git",
        ["-C", &repo, "config", "--get-regexp", "^remote\\..*\\.url$"],
    )?;
    output
        .lines()
        .try_fold(BTreeMap::new(), |mut remotes, line| {
            let (key, url) = line.split_once(' ').ok_or_else(|| JjError::Parse {
                detail: line.to_owned(),
            })?;
            let name = key
                .strip_prefix("remote.")
                .and_then(|value| value.strip_suffix(".url"))
                .ok_or_else(|| JjError::Parse {
                    detail: line.to_owned(),
                })?;
            remotes.insert(name.to_owned(), url.to_owned());
            Ok(remotes)
        })
}

pub(crate) enum OriginTrunk {
    NotRepository,
    Missing,
    Reference(String),
}

pub(crate) fn origin_trunk(consumer: &Path) -> Result<OriginTrunk, JjError> {
    let consumer = path(consumer);
    let Some(inside_work_tree) = command_or_none(command(
        "git",
        ["-C", &consumer, "rev-parse", "--is-inside-work-tree"],
    ))?
    else {
        return Ok(OriginTrunk::NotRepository);
    };
    if inside_work_tree.trim() != "true" {
        return Ok(OriginTrunk::NotRepository);
    }

    if let Some(branch) = command_or_none(command(
        "git",
        ["-C", &consumer, "rev-parse", "--abbrev-ref", "origin/HEAD"],
    ))? {
        let branch = branch.trim();
        if branch.starts_with("origin/") && branch != "origin/HEAD" {
            return Ok(OriginTrunk::Reference(branch.to_owned()));
        }
    }

    for branch in ["origin/main", "origin/master"] {
        if command_or_none(command(
            "git",
            ["-C", &consumer, "rev-parse", "--verify", branch],
        ))?
        .is_some()
        {
            return Ok(OriginTrunk::Reference(branch.to_owned()));
        }
    }
    Ok(OriginTrunk::Missing)
}

/// Reads a consumer pin file from the published default branch so a stale checkout
/// cannot produce a false BEHIND finding.
///
/// Returns `Ok(None)` when the path is not a repository or its origin trunk, ref,
/// or file is absent. Propagates process and parsing failures rather than treating
/// an unavailable Git command as missing consumer data.
pub fn file_at_origin_trunk(
    consumer: &Path,
    file: &str,
) -> Result<Option<(String, usize)>, JjError> {
    let OriginTrunk::Reference(branch) = origin_trunk(consumer)? else {
        return Ok(None);
    };
    file_at_ref(consumer, &branch, file)
}

pub(crate) fn file_at_ref(
    consumer: &Path,
    branch: &str,
    file: &str,
) -> Result<Option<(String, usize)>, JjError> {
    let consumer = path(consumer);
    let revision = format!("{branch}:{file}");
    let Some(content) = command_or_none(command("git", ["-C", &consumer, "show", &revision]))?
    else {
        return Ok(None);
    };
    let behind = match command(
        "git",
        ["-C", &consumer, "rev-list", "--count", branch, "^HEAD"],
    ) {
        Ok(behind) => behind,
        Err(JjError::Command { .. }) => return Ok(Some((content, 0))),
        Err(error) => return Err(error),
    };
    let behind = behind
        .trim()
        .parse::<usize>()
        .map_err(|error| JjError::Parse {
            detail: error.to_string(),
        })?;
    Ok(Some((content, behind)))
}

/// Uses jj porcelain because jj-lib's tree-diff iterator exposes repository paths, not CLI-normalized strings.
pub fn changed_files(repo: &Path, revision: &str) -> Result<Vec<String>, JjError> {
    changed_files_for_diff_args(repo, &["-r", revision])
}

/// Files that differ between two commits.
///
/// A tree diff, not a revset range. `jj diff -r 'A..B'` fails with "Cannot diff revsets with
/// gaps in" whenever B is not a clean descendant of A, which on a fork is the common case;
/// `--from`/`--to` compares two trees and always has an answer.
pub fn changed_files_between(repo: &Path, from: &str, to: &str) -> Result<Vec<String>, JjError> {
    changed_files_for_diff_args(repo, &["--from", from, "--to", to])
}

fn changed_files_for_diff_args(repo: &Path, diff_args: &[&str]) -> Result<Vec<String>, JjError> {
    let repo_path = path(repo);
    let mut args = vec![
        "--repository",
        repo_path.as_str(),
        "--ignore-working-copy",
        "diff",
        "--name-only",
    ];
    args.extend_from_slice(diff_args);
    let output = Command::new("jj")
        .current_dir(repo)
        .args(args)
        .output()
        .map_err(|error| JjError::Process {
            program: "jj".to_owned(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(JjError::Command {
            program: "jj".to_owned(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let output = String::from_utf8(output.stdout).map_err(|error| JjError::Parse {
        detail: error.to_string(),
    })?;
    Ok(output
        .lines()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// Array form, for the common fixed-shape invocation.
fn command<const N: usize>(program: &str, args: [&str; N]) -> Result<String, JjError> {
    command_args(program, &args)
}

/// Slice form, for invocations whose length is not known at compile time, such
/// as a merge over a variable number of parents.
/// Both streams, for the few commands whose useful output is on stderr.
fn command_output(program: &str, args: &[&str]) -> Result<(String, String), JjError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| JjError::Process {
            program: program.to_owned(),
            detail: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(JjError::Command {
            program: format!("{program} {}", args.join(" ")),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Commit ids `jj duplicate` reports creating.
///
/// Identity, not set difference. A set difference over `children(onto)` also
/// captures commits this probe did not create: a dirty `@` that is a child of
/// `onto` gets its commit id rewritten by any snapshotting command, and a
/// concurrent `jj new` by another agent adds one outright. Abandoning that
/// difference destroyed three commits and two bookmarks of another agent's work
/// in a reproduction. Only ids jj says it made are ever abandoned.
pub fn parse_duplicated(stderr: &str) -> Vec<CommitId> {
    stderr
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Duplicated "))
        .filter_map(|rest| rest.split(" as ").nth(1))
        .filter_map(|tail| tail.split_whitespace().nth(1))
        .map(CommitId::new)
        .collect()
}

fn command_args(program: &str, args: &[&str]) -> Result<String, JjError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| JjError::Process {
            program: program.to_owned(),
            detail: error.to_string(),
        })?;
    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|error| JjError::Parse {
            detail: error.to_string(),
        })
    } else {
        Err(JjError::Command {
            program: program.to_owned(),
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn command_or_none(result: Result<String, JjError>) -> Result<Option<String>, JjError> {
    match result {
        Ok(output) => Ok(Some(output)),
        Err(JjError::Command { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

fn commit_id(id: &JjCommitId) -> CommitId {
    CommitId::new(id.to_string())
}

fn path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

struct ProbeCleanup<'a> {
    repo: &'a Path,
    /// Exactly what this probe created, by id.
    created: Vec<CommitId>,
}

impl Drop for ProbeCleanup<'_> {
    fn drop(&mut self) {
        // Abandon exactly the commits jj told us it created, by id, and in ONE
        // command. One at a time does not work: abandoning a commit rebases its
        // descendants, which rewrites the ids of the later ones, so the second
        // abandon addresses an id that no longer exists and that commit
        // survives. Measured, not reasoned.
        //
        // Never a set difference over a shared query: that also matches a
        // concurrent agent's new commit and a dirty `@` whose id a snapshot
        // rewrote, and abandoning those destroys their work. Never
        // `jj op restore` either, for the same reason.
        if self.created.is_empty() {
            return;
        }
        let revset = self
            .created
            .iter()
            .map(|commit| commit.as_str().to_owned())
            .collect::<Vec<_>>()
            .join("|");
        let output = Command::new("jj")
            .args([
                "--repository",
                &path(self.repo),
                "--ignore-working-copy",
                "abandon",
                "-r",
                &revset,
            ])
            .output();
        // A failed cleanup is the one failure this guard exists to report.
        match output {
            Ok(done) if done.status.success() => {}
            Ok(done) => eprintln!(
                "knives: could not abandon probe commits {revset}: {}",
                String::from_utf8_lossy(&done.stderr).trim()
            ),
            Err(error) => eprintln!("knives: could not abandon probe commits {revset}: {error}"),
        }
    }
}

/// Create a flat merge of explicit commits and return the new commit.
///
/// Explicit commit ids, never bookmark names: a name can move between the
/// moment a release is planned and the moment it is cut, and the whole point of
/// a dated release is that it pins specific commits.
///
/// Flat by construction. A nested integration node was considered and rejected:
/// it makes dropping a landed parent harder, forces staleness detection to
/// recurse, and destroys the empty-merge invariant that makes a cut verifiable.
pub fn create_merge(repo: &Path, parents: &[CommitId], message: &str) -> Result<CommitId, JjError> {
    let repo_path = path(repo);
    let mut args: Vec<String> = vec![
        "--repository".to_owned(),
        repo_path.clone(),
        // Do not snapshot, and do not move the working copy. Without these two
        // flags this command parks whoever is working in the repo's default
        // workspace on top of the release merge, with their uncommitted edits
        // pending against it. That is verbatim the accident `knives start` exists
        // to prevent, caused by `knives release`. Reproduced in review.
        "--ignore-working-copy".to_owned(),
        "new".to_owned(),
        "--no-edit".to_owned(),
    ];
    args.extend(parents.iter().map(|parent| parent.as_str().to_owned()));
    args.push("-m".to_owned());
    args.push(message.to_owned());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let (_, reported) = command_output("jj", &borrowed)?;

    // Read the id jj reports, never `@`. Reading `@` was both wrong (with
    // --no-edit the working copy does not move) and a race: any concurrent jj
    // command between the create and the read returns someone else's commit,
    // which would then be bookmarked as the release.
    //
    // jj reports a short id; widen it to the full one so a single id width
    // circulates through the rest of the program.
    let short = parse_created(&reported).ok_or(JjError::ProbeRoot)?;
    let full = command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            short.as_str(),
            "-T",
            "commit_id",
        ],
    )?;
    Ok(CommitId::new(full.trim()))
}

/// The commit id `jj new` reports creating.
pub fn parse_created(stderr: &str) -> Option<CommitId> {
    stderr
        .lines()
        .find_map(|line| line.trim().strip_prefix("Created new commit "))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .map(CommitId::new)
}

pub fn set_bookmark(repo: &Path, name: &str, revision: &str) -> Result<(), JjError> {
    let repo_path = path(repo);
    command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "bookmark",
            "set",
            name,
            "-r",
            revision,
        ],
    )?;
    Ok(())
}

/// Move a release bookmark even when its fresh flat merge is sideways.
///
/// [`set_bookmark`] refuses sideways movement to preserve ordinary dated-release history.
/// Call this only after the caller has established that an in-place move is safe: fixed cuts
/// retain the preceding published cut through its remote-tracking ref, while `run_rebase` first
/// checks `repair_effect` so a followed consumer will receive the repair. The failed sideways
/// move that motivated this distinction means these helpers are not interchangeable.
pub fn set_bookmark_anywhere(repo: &Path, name: &str, revision: &str) -> Result<(), JjError> {
    let repo_path = path(repo);
    command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "bookmark",
            "set",
            "--allow-backwards",
            name,
            "-r",
            revision,
        ],
    )?;
    Ok(())
}

/// Bookmarks whose tip descends from this commit, and where they now are.
///
/// A release parent that nothing points at is stale, but "carries no bookmark"
/// is a poor report. The useful answer is which branch that commit belonged to
/// and where it has moved, which is what design asks for. A branch descends from
/// the parent exactly when the branch moved forward past it, so this recovers
/// the information without needing provenance.
pub fn branches_past(
    repo: &Path,
    commit: &CommitId,
) -> Result<Vec<(BranchName, CommitId)>, JjError> {
    let repo_path = path(repo);
    let revset = format!(
        "bookmarks() & descendants({}) & ~{}",
        commit.as_str(),
        commit.as_str()
    );
    let listed = command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            &revset,
            "-T",
            "commit_id ++ \"\\t\" ++ bookmarks ++ \"\\n\"",
        ],
    )?;
    let mut found = Vec::new();
    for line in listed.lines().filter(|line| !line.trim().is_empty()) {
        let Some((tip, names)) = line.split_once('\t') else {
            continue;
        };
        for raw in names.split_whitespace() {
            // Local names only: a remote-tracking ref moving says origin moved,
            // which is a different fact.
            if !raw.contains('@') {
                let name = raw.trim_end_matches(['*', '?']);
                if !name.is_empty() {
                    found.push((BranchName::new(name), CommitId::new(tip.trim())));
                }
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// The git repository jj keeps its objects in.
fn backing_git_dir(repo: &Path) -> Result<PathBuf, JjError> {
    let store = repo.join(".jj/repo/store");
    let target =
        std::fs::read_to_string(store.join("git_target")).map_err(|error| JjError::Process {
            program: "read git_target".to_owned(),
            detail: error.to_string(),
        })?;
    let target = target.trim();
    Ok(if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        store.join(target)
    })
}

/// Fetch one pull request's head and make it a local bookmark `pull/<n>`.
///
/// Needed to carry someone else's pull request as a release parent: without the
/// objects locally the commit cannot be a merge parent at all. Measured, because
/// none of the obvious routes work. `jj git fetch` brings branches only, so the
/// commit stays invisible. Fetching into `refs/pull/N/head` inside jj's backing
/// store does not help either: jj imports branches and tags, not pull refs, so
/// the commit remains unresolvable. Fetching into a branch-shaped ref and then
/// importing does work, and the result is usable as a parent.
///
/// This writes a local bookmark. It never writes to a remote.
pub fn fetch_pull_ref(repo: &Path, remote_url: &str, number: u64) -> Result<CommitId, JjError> {
    let git_dir = backing_git_dir(repo)?;
    let refspec = format!("refs/pull/{number}/head:refs/heads/pull/{number}");
    let git_dir_arg = git_dir.display().to_string();
    let _ = command(
        "git",
        ["--git-dir", &git_dir_arg, "fetch", remote_url, &refspec],
    )?;
    let _ = command(
        "jj",
        [
            "--repository",
            &path(repo),
            "--ignore-working-copy",
            "git",
            "import",
        ],
    )?;
    let found = command(
        "jj",
        [
            "--repository",
            &path(repo),
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            &format!("pull/{number}"),
            "-T",
            "commit_id",
        ],
    )?;
    Ok(CommitId::new(found.trim()))
}

pub fn forget_workspace(repo: &Path, name: &str) -> Result<(), JjError> {
    let _ = command(
        "jj",
        [
            "--repository",
            &path(repo),
            "--ignore-working-copy",
            "workspace",
            "forget",
            name,
        ],
    )?;
    Ok(())
}

/// Run a command with a revision checked out, and return its output.
///
/// A temporary workspace rather than moving `@`: these repositories are worked
/// concurrently, and checking something out under another agent is the accident
/// this tool exists to prevent. Workspaces are cheap, well under a second,
/// because tracked content is small even in a large checkout.
pub fn output_at_revision(
    repo: &Path,
    revision: &str,
    shell_command: &str,
) -> Result<String, JjError> {
    let name = format!("knives-measure-{}", std::process::id());
    let destination = repo.parent().unwrap_or(repo).join(format!(".{name}"));
    add_workspace(repo, &name, &destination, revision)?;

    let result = Command::new("sh")
        .args(["-c", shell_command])
        .current_dir(&destination)
        .output();

    // Always clean up, on every path.
    let _ = forget_workspace(repo, &name);
    let _ = std::fs::remove_dir_all(&destination);

    let output = result.map_err(|error| JjError::Process {
        program: shell_command.to_owned(),
        detail: error.to_string(),
    })?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// Files left conflicted by a merge.
pub fn conflicted_files(repo: &Path, revision: &str) -> Result<Vec<String>, JjError> {
    // Run WITH the repo as the working directory, so jj prints repo-relative
    // paths. Invoked via `--repository` from elsewhere it prints them relative
    // to the caller's cwd, which produced paths like `../../../../tmp/...`.
    let output = Command::new("jj")
        .args(["--ignore-working-copy", "resolve", "--list", "-r", revision])
        .current_dir(repo)
        .output()
        .map_err(|error| JjError::Process {
            program: "jj resolve --list".to_owned(),
            detail: error.to_string(),
        })?;
    // A non-zero exit means there is nothing to resolve, which is the common and
    // healthy case rather than a failure.
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(ToOwned::to_owned)
        .collect())
}
