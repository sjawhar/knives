use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use futures_core::Stream as _;
use jj_lib::backend::CommitId as JjCommitId;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::local_working_copy::LocalWorkingCopy;
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef};
use jj_lib::ref_name::{RefName as JjRefName, RemoteName as JjRemoteName};
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _, RepoLoader, StoreFactories};
use jj_lib::revset::{SymbolResolver, walk_revs};
use jj_lib::rewrite::{duplicate_commits, merge_commit_trees, rebase_commit};
use jj_lib::settings::UserSettings;
use jj_lib::transaction::Transaction;
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
    #[error("the landed probe for `{branch}` panicked")]
    ProbePanic { branch: String },
    #[error("could not parse command output: {detail}")]
    Parse { detail: String },
    #[error("commit {commit} is immutable: pinned by {pin}")]
    Immutable { commit: String, pin: String },
}

/// Bound the passive operation walk so status stays proportional to current work.
pub const MAX_ACTIVITY_OPS: usize = 200;

/// Twelve characters is what jj shows, and a full id is correct and unreadable.
fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

#[derive(Debug)]
pub struct Repo {
    repo: Arc<ReadonlyRepo>,
    path: PathBuf,
}

/// Working-copy moves for the requested workspaces and the walk's coverage.
///
/// Read-only commands on a clean tree write no operation, and mutations that do
/// not move a working copy cannot be attributed to a workspace. This remains a
/// descriptive observation, never a liveness guarantee.
#[derive(Debug, Default)]
pub struct WorkspaceActivity {
    pub moves: BTreeMap<WorkspaceName, jiff::Timestamp>,
    /// The end time of the oldest visited operation, unless the whole op log was
    /// consumed. A bounded walk must carry this witness so callers never call
    /// an unsearched past "never".
    pub horizon: Option<jiff::Timestamp>,
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
        let settings = repo_settings(path)?;
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
        Ok(Self {
            repo,
            path: path.to_owned(),
        })
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

    /// Walks operations backward from this repository's loaded head, attributing
    /// working-copy changes to requested workspace names.
    pub fn workspace_activity(
        &self,
        wanted: &BTreeSet<WorkspaceName>,
        max_ops: usize,
    ) -> Result<WorkspaceActivity, JjError> {
        let head = self.repo.operation();
        let mut stream = std::pin::pin!(jj_lib::op_walk::walk_ancestors(
            std::slice::from_ref(head)
        ));
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut activity = WorkspaceActivity::default();
        let mut visited = 0;
        let mut exhausted = false;

        loop {
            if visited == max_ops {
                match stream.as_mut().poll_next(&mut context) {
                    Poll::Ready(None) => exhausted = true,
                    Poll::Ready(Some(_)) => {}
                    Poll::Pending => {
                        std::thread::yield_now();
                        continue;
                    }
                }
                break;
            }
            if !wanted.is_empty()
                && wanted
                    .iter()
                    .all(|workspace| activity.moves.contains_key(workspace))
            {
                break;
            }
            match stream.as_mut().poll_next(&mut context) {
                Poll::Ready(Some(Ok(operation))) => {
                    visited += 1;
                    let timestamp = jiff::Timestamp::from_millisecond(
                        operation.metadata().time.end.timestamp.0,
                    )
                    .map_err(|error| JjError::Parse {
                        detail: error.to_string(),
                    })?;
                    activity.horizon = Some(timestamp);
                    if operation.parent_ids().len() != 1 {
                        continue;
                    }

                    let parents = block_on(operation.parents()).map_err(|error| JjError::Parse {
                        detail: error.to_string(),
                    })?;
                    let parent = parents
                        .into_iter()
                        .next()
                        .expect("one parent id yields one parent operation");
                    let view = block_on(operation.view()).map_err(|error| JjError::Parse {
                        detail: error.to_string(),
                    })?;
                    let parent_view = block_on(parent.view()).map_err(|error| JjError::Parse {
                        detail: error.to_string(),
                    })?;
                    for (name, commit) in view.wc_commit_ids() {
                        let workspace = WorkspaceName::new(name.as_symbol().to_string());
                        if wanted.contains(&workspace)
                            && !activity.moves.contains_key(&workspace)
                            && parent_view.wc_commit_ids().get(name) != Some(commit)
                        {
                            activity.moves.insert(workspace, timestamp);
                        }
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    return Err(JjError::Parse {
                        detail: error.to_string(),
                    });
                }
                Poll::Ready(None) => {
                    exhausted = true;
                    break;
                }
                Poll::Pending => std::thread::yield_now(),
            }
        }
        if exhausted {
            activity.horizon = None;
        }
        Ok(activity)
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

    /// The full description of `revision`.
    ///
    /// Release cuts record member provenance in their description, so the next
    /// cut can ask what its predecessor carried long after the member
    /// bookmarks have moved or gone.
    pub fn description_of(&self, revision: &str) -> Result<String, JjError> {
        Ok(self.commit(revision)?.description().to_owned())
    }

    /// One change existing as several visible commits, ignoring nominated refs.
    ///
    /// Every candidate comes from the existing jj-lib index, walking ancestors
    /// of the heads whose references still vouch for them. That preserves a
    /// copy buried under descendants while avoiding a porcelain `divergent()`
    /// scan across unrelated visible history.
    ///
    /// `ignored` names refs whose testimony does not count — in practice the
    /// superseded dated releases, which any `jj git fetch` re-materializes as
    /// untracked refs forever (they exist on the remote and jj keeps no memory of
    /// forgetting them). A head every one of whose refs is ignored cannot vouch for
    /// a divergent copy, which is only reported while some non-ignored head can reach it.
    /// Filtering the reader instead of re-cleaning the graph is deliberate: the
    /// repo must stay correct under bare fetches by any tool.
    pub fn divergent_changes(
        &self,
        ignored: &BTreeSet<BookmarkRef>,
    ) -> Result<Vec<(ChangeId, CommitId)>, JjError> {
        let tips = self.bookmark_tips()?;
        // Refs per commit, so "every ref on this head is ignored" is answerable.
        let mut refs_at: BTreeMap<&CommitId, Vec<&BookmarkRef>> = BTreeMap::new();
        for (reference, commit) in &tips {
            refs_at.entry(commit).or_default().push(reference);
        }
        // Kept heads are the vouching authorities: a copy counts only while a
        // head that is not ignored-only can reach it.
        let mut kept_heads = Vec::new();
        for head in self.repo.view().heads() {
            let commit = commit_id(head);
            let all_ignored = refs_at
                .get(&commit)
                .is_some_and(|refs| refs.iter().all(|reference| ignored.contains(reference)));
            if !all_ignored {
                kept_heads.push(head.clone());
            }
        }

        let candidates =
            walk_revs(self.repo.as_ref(), &kept_heads, &[]).map_err(|error| JjError::Revision {
                revision: "divergent changes".to_owned(),
                detail: error.to_string(),
            })?;
        let mut changes = BTreeMap::<ChangeId, BTreeSet<CommitId>>::new();
        for candidate in collect_stream(candidates.commit_change_ids()) {
            let (commit, change) = candidate.map_err(|error| JjError::Revision {
                revision: "divergent changes".to_owned(),
                detail: error.to_string(),
            })?;
            changes
                .entry(ChangeId::new(change.to_string()))
                .or_default()
                .insert(commit_id(&commit));
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
        self.backend_is_ancestor(ancestor.id(), descendant.id())
    }

    /// Compares a revision's net tree change with a target in one three-way
    /// merge. Unlike a per-commit rebase, this correctly recognizes a squashed
    /// series and a branch whose additions are later reverted.
    pub fn tree_replay_outcome(
        &self,
        revision: &CommitId,
        target: &CommitId,
    ) -> Result<RebaseOutcome, JjError> {
        let base = self
            .common_ancestor(std::slice::from_ref(revision), target)?
            .ok_or_else(|| JjError::Revision {
                revision: revision.as_str().to_owned(),
                detail: format!("no unique common ancestor with {}", target.as_str()),
            })?;
        let base = self.commit(base.as_str())?;
        let target = self.commit(target.as_str())?;
        let revision = self.commit(revision.as_str())?;
        let merged = block_on(MergedTree::merge(Merge::from_vec(vec![
            (target.tree(), "target".to_owned()),
            (base.tree(), "base".to_owned()),
            (revision.tree(), "revision".to_owned()),
        ])))
        .map_err(|error| store_error(&error))?;

        Ok(if merged.has_conflict() {
            RebaseOutcome::Conflicted
        } else if merged.tree_ids() == target.tree_ids() {
            RebaseOutcome::Empty
        } else {
            RebaseOutcome::CleanNonEmpty
        })
    }

    /// [`Self::is_ancestor`] by backend id, saving the hex round-trip.
    fn backend_is_ancestor(
        &self,
        ancestor: &JjCommitId,
        descendant: &JjCommitId,
    ) -> Result<bool, JjError> {
        self.repo
            .index()
            .is_ancestor(ancestor, descendant)
            .map_err(|error| JjError::Revision {
                revision: descendant.to_string(),
                detail: error.to_string(),
            })
    }

    /// Commits of `base..tip` — reachable from `tip` but not from `base` —
    /// children before parents: the order [`duplicate_commits`] requires.
    fn range_newest_first(
        &self,
        base: &JjCommitId,
        tip: &JjCommitId,
    ) -> Result<Vec<JjCommitId>, JjError> {
        let mut oldest_first = Vec::new();
        let mut visited = BTreeSet::new();
        let mut stack = vec![(tip.clone(), false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                oldest_first.push(id);
                continue;
            }
            if visited.contains(&id) || self.backend_is_ancestor(&id, base)? {
                continue;
            }
            visited.insert(id.clone());
            let commit = self
                .repo
                .store()
                .get_commit(&id)
                .map_err(|error| store_error(&error))?;
            stack.push((id, true));
            for parent in commit.parent_ids() {
                if !visited.contains(parent) {
                    stack.push((parent.clone(), false));
                }
            }
        }
        oldest_first.reverse();
        Ok(oldest_first)
    }

    /// The single newest commit that every one of `commits` and `tip` can all
    /// reach: their common fork point. `None` when the histories criss-cross to
    /// several candidates — rare enough that callers keep their own fallback
    /// rather than have one guessed here.
    pub fn common_ancestor(
        &self,
        commits: &[CommitId],
        tip: &CommitId,
    ) -> Result<Option<CommitId>, JjError> {
        let mut common = vec![self.commit(tip.as_str())?.id().clone()];
        for commit in commits {
            let commit = self.commit(commit.as_str())?.id().clone();
            common = self
                .repo
                .index()
                .common_ancestors(&common, std::slice::from_ref(&commit))
                .map_err(|error| JjError::Revision {
                    revision: commit.to_string(),
                    detail: error.to_string(),
                })?;
        }
        match common.as_slice() {
            [only] => Ok(Some(commit_id(only))),
            _ => Ok(None),
        }
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
        publish_remote: &str,
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
            if is_our_release(&reference, scheme, publish_remote)
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
    let args = ls_remote_args(remote_url, &["refs/pull/*/head"]);
    let output = command_args("git", &args)?;
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

/// Live remote refs by pattern: one ls-remote round trip. The remote's truth,
/// not the last fetch's — reconciliation must compare against what is there NOW.
pub fn remote_refs(
    remote_url: &str,
    patterns: &[&str],
) -> Result<BTreeMap<String, CommitId>, JjError> {
    let args = ls_remote_args(remote_url, patterns);
    let output = command_args("git", &args)?;
    output.lines().try_fold(BTreeMap::new(), |mut refs, line| {
        let (sha, reference) = line.split_once('\t').ok_or_else(|| JjError::Parse {
            detail: line.to_owned(),
        })?;
        refs.insert(reference.to_owned(), CommitId::new(sha));
        Ok(refs)
    })
}

fn ls_remote_args<'a>(remote_url: &'a str, patterns: &[&'a str]) -> Vec<&'a str> {
    let mut args = Vec::with_capacity(patterns.len() + 3);
    args.extend(["ls-remote", "--", remote_url]);
    args.extend(patterns.iter().copied());
    args
}

/// Replays a branch onto `onto` as a dropped-transaction read: nothing is written.
pub fn probe_landed(
    repo: &Path,
    branch: &BranchName,
    onto: &str,
) -> Result<RebaseOutcome, JjError> {
    probe_revision(repo, onto, branch.as_str(), onto)
}

/// Whether any commit in `base..tip` is non-empty: work the base does not have.
///
/// An empty range carries nothing — a tip the base already contains needs no
/// replay to answer. A rebased-but-landed chain is all empty commits, so this
/// one reading covers merge-commit, squash and rebase landings alike.
pub fn carries_work_past(repo: &Path, base: &CommitId, tip: &CommitId) -> Result<bool, JjError> {
    let repo_path = path(repo);
    let range = format!("{}..{}", base.as_str(), tip.as_str());
    let states = command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            &range,
            "-T",
            "empty ++ \"\\n\"",
        ],
    )?;
    Ok(states.lines().any(|line| line.trim() == "false"))
}

/// Replays the net tree effect of `base..revision` onto a target.
///
/// A scratch child of `base` carrying `revision`'s tree has the range's net
/// effect as its diff by construction. Replaying that one synthetic commit
/// avoids the conflicts that replaying each source commit manufactures when an
/// intermediate commit is applied to a tree that already has the range's final
/// content.
///
/// A read, not a mutation: the replay runs inside a jj-lib transaction that is
/// dropped without ever becoming an operation (#18). No other reader of the
/// shared repository can observe it at any point, so there are no synthetic
/// bookmarks, no cleanup guard, and no crash-recovery machinery.
pub fn probe_net_diff(
    repo: &Path,
    base: &str,
    revision: &str,
    onto: &str,
) -> Result<RebaseOutcome, JjError> {
    let repo = Repo::open(repo)?;
    let base = repo.commit(base)?;
    let revision = repo.commit(revision)?;
    let onto = repo.commit(onto)?;
    // `base..revision` is empty exactly when the base already reaches the tip.
    if repo.backend_is_ancestor(revision.id(), base.id())? {
        return Ok(RebaseOutcome::Empty);
    }
    let mut tx = repo.repo.start_transaction();
    let synthetic = block_on(
        tx.repo_mut()
            .new_commit(vec![base.id().clone()], revision.tree())
            .write(),
    )
    .map_err(|error| store_error(&error))?;
    let replayed = block_on(rebase_commit(
        tx.repo_mut(),
        synthetic,
        vec![onto.id().clone()],
    ))
    .map_err(|error| store_error(&error))?;
    let outcome = if replayed.has_conflict() {
        RebaseOutcome::Conflicted
    } else if block_on(replayed.is_empty(tx.repo())).map_err(|error| store_error(&error))? {
        RebaseOutcome::Empty
    } else {
        RebaseOutcome::CleanNonEmpty
    };
    // Dropped, never committed: the probe leaves no trace in the op log.
    drop(tx);
    Ok(outcome)
}

/// Replays `base..revision` onto a target and classifies the resulting content.
///
/// A read, not a mutation, in the same dropped-transaction style as
/// [`probe_net_diff`]. [`duplicate_commits`] is the code `jj duplicate -r
/// <range> -d <onto>` itself runs, so replay semantics are unchanged from the
/// porcelain implementation this replaces.
pub fn probe_revision(
    repo: &Path,
    base: &str,
    revision: &str,
    onto: &str,
) -> Result<RebaseOutcome, JjError> {
    let repo = Repo::open(repo)?;
    let base = repo.commit(base)?;
    let revision = repo.commit(revision)?;
    let onto = repo.commit(onto)?;
    let targets = repo.range_newest_first(base.id(), revision.id())?;
    if targets.is_empty() {
        // Nothing to replay: the range holds no commit that `onto` lacks, so
        // its content is already there. A merge without squashing lands here.
        return Ok(RebaseOutcome::Empty);
    }
    let mut tx = repo.repo.start_transaction();
    let stats = block_on(duplicate_commits(
        tx.repo_mut(),
        &targets,
        &std::collections::HashMap::new(),
        std::slice::from_ref(onto.id()),
        &[],
    ))
    .map_err(|error| store_error(&error))?;
    // Every duplicated commit, not just the first. A branch of several commits
    // duplicates as several, and judging the branch by one of them answers a
    // different question.
    let mut conflicted = false;
    let mut all_empty = true;
    for replayed in stats.duplicated_commits.values() {
        conflicted = conflicted || replayed.has_conflict();
        all_empty = all_empty
            && block_on(replayed.is_empty(tx.repo())).map_err(|error| store_error(&error))?;
    }
    drop(tx);
    if stats.duplicated_commits.is_empty() {
        return Err(JjError::ProbeRoot);
    }
    Ok(if conflicted {
        RebaseOutcome::Conflicted
    } else if all_empty {
        RebaseOutcome::Empty
    } else {
        RebaseOutcome::CleanNonEmpty
    })
}

/// A backend failure while probing, named for the store that failed.
fn store_error(error: &jj_lib::backend::BackendError) -> JjError {
    JjError::Open {
        path: "commit store".to_owned(),
        detail: error.to_string(),
    }
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

/// Runs a dynamically shaped command and returns both output streams on success.
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

/// One release write — a fresh flat merge or a duplicate of a prior cut —
/// applied and bookmarked as a single described operation.
#[derive(Debug)]
pub struct ReleaseWrite<'a> {
    /// Duplicate this commit onto the new parents; `None` builds a fresh flat
    /// merge of exactly `parents`.
    pub source: Option<&'a CommitId>,
    pub parents: &'a [CommitId],
    /// `None` keeps the source's own message (fresh merges must name one).
    pub message: Option<&'a str>,
    /// Point this bookmark at the written commit, sideways moves allowed —
    /// callers that pass one have already established the move is safe (see
    /// [`set_bookmark_anywhere`]).
    pub bookmark: Option<&'a str>,
    /// The operation-log description, `knives: …` by convention.
    pub operation: &'a str,
}

/// Write a release commit — and optionally its bookmark — as ONE operation.
///
/// Explicit commit ids, never bookmark names: a name can move between the
/// moment a release is planned and the moment it is cut, and the whole point of
/// a dated release is that it pins specific commits.
///
/// Flat by construction. A nested integration node was considered and rejected:
/// it makes dropping a landed parent harder, forces staleness detection to
/// recurse, and destroys the empty-merge invariant that makes a cut verifiable.
///
/// An empty parent set is refused rather than passed on: a caller that computed
/// its way to no parents would report the change it meant to make while the
/// composition stayed exactly as it was.
///
/// Never touches a working copy. The porcelain this replaces (`jj new
/// --no-edit`, `jj duplicate`) parked whoever was working in the default
/// workspace on top of the release merge unless flagged off; a jj-lib write
/// cannot make that mistake, and it reports the created commit directly
/// instead of through parsed human-facing output.
pub fn write_release(repo: &Path, write: &ReleaseWrite<'_>) -> Result<CommitId, JjError> {
    validate_parents(write.source, write.parents)?;
    let repo = Repo::open(repo)?;
    let (parent_commits, parent_ids) = resolved_parents(&repo, write.parents)?;
    let mut tx = repo.repo.start_transaction();
    let written = if let Some(source) = write.source {
        let source = repo.commit(source.as_str())?;
        duplicated_release(&mut tx, (&source, write.message), &parent_ids)?
    } else {
        merged_release(
            &mut tx,
            (&parent_commits, parent_ids),
            write.message.unwrap_or_default(),
        )?
    };
    if let Some(name) = write.bookmark {
        tx.repo_mut().set_local_bookmark_target(
            JjRefName::new(name),
            RefTarget::normal(written.id().clone()),
        );
    }
    commit_mutation(&repo, tx, write.operation)?;
    Ok(commit_id(written.id()))
}

/// Refuse an empty or repeated parent set before anything is written.
///
/// The porcelain silently DEDUPED a repeated parent, which is how "a branch's
/// work was dropped" once looked; a jj-lib write would happily record the
/// duplicate instead. Both are wrong: the composition is the parent set, so a
/// repeat is always a caller bug.
fn validate_parents(source: Option<&CommitId>, parents: &[CommitId]) -> Result<(), JjError> {
    if parents.is_empty() {
        return Err(JjError::Revision {
            revision: source.map_or_else(
                || "a fresh merge".to_owned(),
                |source| source.as_str().to_owned(),
            ),
            detail: "a release write needs at least one destination parent".to_owned(),
        });
    }
    let mut seen = BTreeSet::new();
    for parent in parents {
        if !seen.insert(parent.as_str()) {
            return Err(JjError::Revision {
                revision: parent.as_str().to_owned(),
                detail: "a release cannot carry the same parent twice".to_owned(),
            });
        }
    }
    Ok(())
}

/// Every parent as a resolved commit, alongside its backend id.
fn resolved_parents(
    repo: &Repo,
    parents: &[CommitId],
) -> Result<(Vec<jj_lib::commit::Commit>, Vec<JjCommitId>), JjError> {
    let commits: Vec<jj_lib::commit::Commit> = parents
        .iter()
        .map(|parent| repo.commit(parent.as_str()))
        .collect::<Result<_, _>>()?;
    let ids = commits.iter().map(|commit| commit.id().clone()).collect();
    Ok((commits, ids))
}

/// A fresh flat merge of the parents, described.
fn merged_release(
    tx: &mut Transaction,
    (parent_commits, parent_ids): (&[jj_lib::commit::Commit], Vec<JjCommitId>),
    message: &str,
) -> Result<jj_lib::commit::Commit, JjError> {
    let tree = block_on(merge_commit_trees(tx.repo(), parent_commits))
        .map_err(|error| store_error(&error))?;
    block_on(
        tx.repo_mut()
            .new_commit(parent_ids, tree)
            .set_description(message)
            .write(),
    )
    .map_err(|error| store_error(&error))
}

/// The duplicate of `source` onto `parent_ids`, message optionally replaced.
fn duplicated_release(
    tx: &mut Transaction,
    (source, message): (&jj_lib::commit::Commit, Option<&str>),
    parent_ids: &[JjCommitId],
) -> Result<jj_lib::commit::Commit, JjError> {
    let mut descriptions = std::collections::HashMap::new();
    if let Some(message) = message {
        descriptions.insert(source.id().clone(), message.to_owned());
    }
    let stats = block_on(duplicate_commits(
        tx.repo_mut(),
        std::slice::from_ref(source.id()),
        &descriptions,
        parent_ids,
        &[],
    ))
    .map_err(|error| store_error(&error))?;
    stats
        .duplicated_commits
        .into_values()
        .next()
        .ok_or_else(|| JjError::Revision {
            revision: source.id().to_string(),
            detail: "the duplicate produced no commit".to_owned(),
        })
}

/// What one cut is made of, owned so a candidate can be rebuilt verbatim.
#[derive(Debug, Clone)]
pub struct CutSpec {
    /// Duplicate this commit onto the parents; `None` builds a fresh flat merge.
    pub source: Option<CommitId>,
    pub parents: Vec<CommitId>,
    pub message: String,
}

/// A release commit built in a scratch transaction that is never committed.
///
/// The cut audit needs to READ the merge it is judging. Committing the merge
/// first meant a failed audit had to compensate with an abandon, and a crash
/// between the two operations stranded an anonymous merge. A candidate gives
/// the audit a real commit to read — conflicts, member replays, tree drift —
/// inside a transaction that simply evaporates afterwards: a failed audit
/// writes nothing at all. [`Candidate::publish`] rebuilds the spec in a fresh
/// transaction and refuses if the rebuilt tree differs from the audited one,
/// so the verdict provably applies to what ships.
pub struct Candidate {
    repo: Repo,
    tx: Transaction,
    spec: CutSpec,
    commit: jj_lib::commit::Commit,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Candidate")
            .field("spec", &self.spec)
            .field("commit", self.commit.id())
            .finish_non_exhaustive()
    }
}

/// Build `spec`'s release commit in a scratch transaction, for auditing.
pub fn candidate_release(repo: &Path, spec: CutSpec) -> Result<Candidate, JjError> {
    validate_parents(spec.source.as_ref(), &spec.parents)?;
    let repo = Repo::open(repo)?;
    let mut tx = repo.repo.start_transaction();
    let commit = spec_commit(&repo, &mut tx, &spec)?;
    Ok(Candidate {
        repo,
        tx,
        spec,
        commit,
    })
}

/// The release commit `spec` describes, written into `tx`.
fn spec_commit(
    repo: &Repo,
    tx: &mut Transaction,
    spec: &CutSpec,
) -> Result<jj_lib::commit::Commit, JjError> {
    let (parent_commits, parent_ids) = resolved_parents(repo, &spec.parents)?;
    if let Some(source) = &spec.source {
        let source = repo.commit(source.as_str())?;
        duplicated_release(tx, (&source, Some(&spec.message)), &parent_ids)
    } else {
        merged_release(tx, (&parent_commits, parent_ids), &spec.message)
    }
}

impl Candidate {
    pub fn commit_id(&self) -> CommitId {
        commit_id(self.commit.id())
    }

    pub fn parent_count(&self) -> usize {
        self.commit.parent_ids().len()
    }

    /// Files the candidate's tree leaves conflicted.
    pub fn conflicted_files(&self) -> Result<Vec<String>, JjError> {
        let mut files = Vec::new();
        for (file, value) in self.commit.tree().conflicts() {
            value.map_err(|error| store_error(&error))?;
            files.push(file.as_internal_file_string().to_owned());
        }
        Ok(files)
    }

    /// [`probe_net_diff`] with the candidate as the target: the net effect of
    /// `base..revision`, replayed onto the candidate in its own scratch
    /// transaction.
    pub fn replay_outcome(&mut self, base: &str, revision: &str) -> Result<RebaseOutcome, JjError> {
        let base = self.repo.commit(base)?;
        let revision = self.repo.commit(revision)?;
        // `base..revision` is empty exactly when the base already reaches the tip.
        if self.repo.backend_is_ancestor(revision.id(), base.id())? {
            return Ok(RebaseOutcome::Empty);
        }
        let synthetic = block_on(
            self.tx
                .repo_mut()
                .new_commit(vec![base.id().clone()], revision.tree())
                .write(),
        )
        .map_err(|error| store_error(&error))?;
        let replayed = block_on(rebase_commit(
            self.tx.repo_mut(),
            synthetic,
            vec![self.commit.id().clone()],
        ))
        .map_err(|error| store_error(&error))?;
        Ok(if replayed.has_conflict() {
            RebaseOutcome::Conflicted
        } else if block_on(replayed.is_empty(self.tx.repo()))
            .map_err(|error| store_error(&error))?
        {
            RebaseOutcome::Empty
        } else {
            RebaseOutcome::CleanNonEmpty
        })
    }

    /// Paths whose content differs between `previous` and the candidate.
    pub fn changed_files_since(&self, previous: &str) -> Result<Vec<String>, JjError> {
        let previous = self.repo.commit(previous)?;
        let previous_tree = previous.tree();
        let candidate_tree = self.commit.tree();
        let mut files = Vec::new();
        for entry in collect_stream(previous_tree.diff_stream(&candidate_tree, &EverythingMatcher))
        {
            entry.values.map_err(|error| store_error(&error))?;
            files.push(entry.path.as_internal_file_string().to_owned());
        }
        Ok(files)
    }

    /// Rebuild the audited spec in a fresh transaction, point `bookmark` at it,
    /// and publish as ONE operation: creation, audit and naming never exist as
    /// separate published states. Refuses when the rebuilt tree differs from
    /// the audited tree, so the audit's verdict provably applies to what ships.
    pub fn publish(
        self,
        (bookmark, motion): (&str, BookmarkMotion),
        operation: &str,
    ) -> Result<CommitId, JjError> {
        let Self {
            repo,
            tx,
            spec,
            commit: audited,
        } = self;
        // The audited scratch — candidate and probe leftovers — evaporates
        // before anything real is written.
        drop(tx);
        let mut tx = repo.repo.start_transaction();
        let written = spec_commit(&repo, &mut tx, &spec)?;
        if written.tree_ids() != audited.tree_ids() {
            return Err(JjError::Revision {
                revision: commit_id(audited.id()).as_str().to_owned(),
                detail: "the rebuilt cut's tree differs from the audited candidate".to_owned(),
            });
        }
        guard_bookmark_motion(tx.repo(), (bookmark, written.id()), motion)?;
        tx.repo_mut().set_local_bookmark_target(
            JjRefName::new(bookmark),
            RefTarget::normal(written.id().clone()),
        );
        commit_mutation(&repo, tx, operation)?;
        Ok(commit_id(written.id()))
    }
}

/// Drain a jj-lib stream with the same noop-waker loop [`block_on`] uses.
fn collect_stream<S: futures_core::Stream>(stream: S) -> Vec<S::Item> {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut stream = std::pin::pin!(stream);
    let mut items = Vec::new();
    loop {
        match stream.as_mut().poll_next(&mut context) {
            Poll::Ready(Some(item)) => items.push(item),
            Poll::Ready(None) => return items,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A ref jj's stock configuration treats as an immutability pin.
struct ImmutablePin {
    commit: JjCommitId,
    label: String,
}

/// The pins `builtin_immutable_heads()` names under jj's defaults:
/// `present(trunk()) | tags() | untracked_remote_bookmarks()`. Trunk is read
/// wider than jj's alias — a trunk-named bookmark on ANY remote rather than
/// one chosen remote's — which can only refuse more, never less, and no
/// knives verb rewrites trunk ancestry on purpose.
fn immutable_pins(repo: &dyn jj_lib::repo::Repo) -> Vec<ImmutablePin> {
    let mut pins = Vec::new();
    let view = repo.view();
    for (name, targets) in view.tags() {
        for id in targets.local_target.added_ids() {
            pins.push(ImmutablePin {
                commit: id.clone(),
                label: format!("tag {}", name.as_str()),
            });
        }
        for (remote, remote_ref) in targets.remote_refs {
            for id in remote_ref.target.added_ids() {
                pins.push(ImmutablePin {
                    commit: id.clone(),
                    label: format!("tag {}@{}", name.as_str(), remote.as_str()),
                });
            }
        }
    }
    for (name, targets) in view.bookmarks() {
        let trunkish = matches!(name.as_str(), "main" | "master" | "trunk");
        for (remote, remote_ref) in targets.remote_refs {
            if trunkish || !remote_ref.is_tracked() {
                for id in remote_ref.target.added_ids() {
                    pins.push(ImmutablePin {
                        commit: id.clone(),
                        label: format!("{}@{}", name.as_str(), remote.as_str()),
                    });
                }
            }
        }
    }
    pins
}

/// Refuse to rewrite what jj itself would refuse to rewrite.
///
/// The jj CLI enforces `immutable_heads()` on every rewriting command; jj-lib
/// deliberately does not, so this is the library-side equivalent under stock
/// configuration. The reap flow DEPENDS on the refusal: a superseded cut
/// pinned by someone else's untracked remote ref must land in
/// `forgotten_only`, never be abandoned.
fn assert_mutable(
    repo: &dyn jj_lib::repo::Repo,
    targets: &[jj_lib::commit::Commit],
) -> Result<(), JjError> {
    let pins = immutable_pins(repo);
    for target in targets {
        for pin in &pins {
            let pinned = repo
                .index()
                .is_ancestor(target.id(), &pin.commit)
                .map_err(|error| JjError::Revision {
                    revision: target.id().to_string(),
                    detail: error.to_string(),
                })?;
            if pinned {
                return Err(JjError::Immutable {
                    commit: short_id(&target.id().to_string()),
                    pin: pin.label.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Publish one mutation as ONE described operation.
///
/// Descendants are rebased first (bookmarks and parked working copies follow
/// their rewritten commits, as under the CLI), the git refs are exported so
/// `git` and `gh` in a colocated checkout see the result immediately rather
/// than at someone's next jj command, and the transaction commits under
/// `description`. Failure before the commit writes nothing at all.
///
/// Deliberately no git IMPORT here: nothing moves git refs between jj commands
/// in a knives-managed checkout, every jj porcelain run (knives sync included)
/// imports anyway, and import options are behavior configuration — behavior
/// stays at jj's defaults (#18).
fn commit_mutation(repo: &Repo, tx: Transaction, description: &str) -> Result<(), JjError> {
    let mut tx = tx;
    block_on(tx.repo_mut().rebase_descendants()).map_err(|error| store_error(&error))?;
    let stats = jj_lib::git::export_refs(tx.repo_mut()).map_err(|error| JjError::Open {
        path: repo.path.display().to_string(),
        detail: format!("could not export git refs: {error}"),
    })?;
    for (symbol, reason) in stats
        .failed_bookmarks
        .iter()
        .chain(stats.failed_tags.iter())
    {
        eprintln!("knives: could not export {symbol:?} to git: {reason:?}");
    }
    block_on(tx.commit(description)).map_err(|error| JjError::Open {
        path: repo.path.display().to_string(),
        detail: format!("could not commit the operation: {error}"),
    })?;
    Ok(())
}

/// jj's stock configuration, plus the writer identity resolved the way the jj
/// CLI resolves it: `JJ_USER`/`JJ_EMAIL`, then the repository's own
/// `.jj/repo/config.toml`, then the user file (`$JJ_CONFIG` when set,
/// otherwise `$XDG_CONFIG_HOME/jj/config.toml`, `~/.config/jj/config.toml`,
/// `~/.jjconfig.toml`). Only `user.name` and `user.email` are read: every
/// behavioral setting deliberately stays at jj's defaults (#18), but a commit
/// written with an empty author could never be pushed.
fn repo_settings(path: &Path) -> Result<UserSettings, JjError> {
    let open_error = |detail: String| JjError::Open {
        path: path.display().to_string(),
        detail,
    };
    let mut config = StackedConfig::with_defaults();
    let (name, email) = resolved_identity(path)?;
    let mut lines = Vec::new();
    if let Some(name) = name {
        lines.push(format!("user.name = {}", toml_string(&name)));
    }
    if let Some(email) = email {
        lines.push(format!("user.email = {}", toml_string(&email)));
    }
    if !lines.is_empty() {
        let layer = ConfigLayer::parse(ConfigSource::User, &lines.join("\n"))
            .map_err(|error| open_error(error.to_string()))?;
        config.add_layer(layer);
    }
    UserSettings::from_config(config).map_err(|error| open_error(error.to_string()))
}

/// A string as a quoted, escaped TOML literal.
fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

/// The identity the jj CLI would write with, by its precedence.
fn resolved_identity(repo_path: &Path) -> Result<(Option<String>, Option<String>), JjError> {
    let mut name = identity_var("JJ_USER");
    let mut email = identity_var("JJ_EMAIL");
    for file in identity_files(repo_path) {
        if name.is_some() && email.is_some() {
            break;
        }
        let (file_name, file_email) = file_identity(&file)?;
        name = name.or(file_name);
        email = email.or(file_email);
    }
    Ok((name, email))
}

fn identity_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// Identity sources after the environment, highest precedence first.
fn identity_files(repo_path: &Path) -> Vec<PathBuf> {
    let mut files = repo_config_files(repo_path);
    if let Ok(paths) = std::env::var("JJ_CONFIG") {
        // Like jj, `JJ_CONFIG` replaces the user files entirely.
        files.extend(std::env::split_paths(&paths));
        return files;
    }
    if let Some(home) = config_home() {
        files.push(home.join("jj/config.toml"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        files.push(PathBuf::from(home).join(".jjconfig.toml"));
    }
    files
}

/// Where this repository's own jj config may live, newest convention first.
fn repo_config_files(repo_path: &Path) -> Vec<PathBuf> {
    let mut repo_dir = repo_path.join(".jj/repo");
    // A non-default workspace's `.jj/repo` is a file naming the real repo directory.
    if repo_dir.is_file()
        && let Ok(pointed) = std::fs::read_to_string(&repo_dir)
    {
        repo_dir = PathBuf::from(pointed.trim());
    }
    let mut files = Vec::new();
    // Newer jj keeps per-repo config under the user config directory, keyed by
    // the repository's `config-id`.
    if let Ok(id) = std::fs::read_to_string(repo_dir.join("config-id"))
        && !id.trim().is_empty()
        && let Some(home) = config_home()
    {
        files.push(home.join("jj/repos").join(id.trim()).join("config.toml"));
    }
    // Older jj kept it inside the repository.
    files.push(repo_dir.join("config.toml"));
    files
}

/// `$XDG_CONFIG_HOME`, else `~/.config` — the directory jj's user config lives under.
fn config_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".config"))
}

/// `user.name` and `user.email` from one jj config file. An absent or
/// unreadable file is not a source; a file that exists but cannot parse is an
/// error — quietly writing anonymous commits because a config file is broken
/// would surface much later, as an unpushable release.
fn file_identity(file: &Path) -> Result<(Option<String>, Option<String>), JjError> {
    let Ok(text) = std::fs::read_to_string(file) else {
        return Ok((None, None));
    };
    let table: toml::Table = text
        .parse()
        .map_err(|error: toml::de::Error| JjError::Parse {
            detail: format!("{}: {error}", file.display()),
        })?;
    let Some(toml::Value::Table(user)) = table.get("user") else {
        return Ok((None, None));
    };
    Ok((table_string(user, "name"), table_string(user, "email")))
}

fn table_string(table: &toml::Table, key: &str) -> Option<String> {
    match table.get(key) {
        Some(toml::Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

/// Rebase a whole branch onto `dest`: `jj rebase -b rev -d dest`.
///
/// Moves `rev`'s ancestry not already reachable from `dest`, plus all its
/// descendants. Bookmarks and working copies follow their rewritten commits;
/// conflicts are recorded in the commits rather than blocking.
pub fn rebase_branch_onto(repo: &Path, rev: &str, dest: &CommitId) -> Result<(), JjError> {
    // No --ignore-working-copy: this rewrites checked-out ancestry, and the
    // invoking workspace must move with the operation rather than go stale.
    let repo_path = path(repo);
    let _ = command_output(
        "jj",
        &[
            "--repository",
            &repo_path,
            "rebase",
            "-b",
            rev,
            "-d",
            dest.as_str(),
        ],
    )?;
    Ok(())
}

/// Rewrites a commit's message as ONE described operation and returns the
/// replacement commit id. Descendants and bookmarks follow the rewrite, as
/// they do under `jj describe`; immutable commits refuse.
pub fn describe_commit(
    repo: &Path,
    commit: &CommitId,
    message: &str,
    operation: &str,
) -> Result<CommitId, JjError> {
    let repo = Repo::open(repo)?;
    let target = repo.commit(commit.as_str())?;
    assert_mutable(repo.repo.as_ref(), std::slice::from_ref(&target))?;
    let mut tx = repo.repo.start_transaction();
    let rewritten = block_on(
        tx.repo_mut()
            .rewrite_commit(&target)
            .set_description(message)
            .write(),
    )
    .map_err(|error| store_error(&error))?;
    commit_mutation(&repo, tx, operation)?;
    Ok(commit_id(rewritten.id()))
}

/// Point `name` at `revision` as ONE described operation, refusing backwards
/// or sideways movement.
///
/// Dated cuts retain this protection because each name records a new release;
/// [`set_bookmark_anywhere`] is the deliberate override.
pub fn set_bookmark(repo: &Path, name: &str, revision: &str) -> Result<(), JjError> {
    move_bookmark(repo, (name, revision), BookmarkMotion::ForwardOnly)
}

/// Move a release bookmark even when its fresh flat merge is sideways.
///
/// [`set_bookmark`] refuses sideways movement to preserve ordinary dated-release history.
/// Call this only after the caller has established that an in-place move is safe: fixed cuts
/// retain the preceding published cut through its remote-tracking ref, while `run_rebase` first
/// checks `repair_effect` so a followed consumer will receive the repair. The failed sideways
/// move that motivated this distinction means these helpers are not interchangeable.
pub fn set_bookmark_anywhere(repo: &Path, name: &str, revision: &str) -> Result<(), JjError> {
    move_bookmark(repo, (name, revision), BookmarkMotion::Anywhere)
}

/// How far a bookmark may move: [`BookmarkMotion::ForwardOnly`] refuses
/// backwards or sideways movement, [`BookmarkMotion::Anywhere`] is the
/// deliberate override for fixed release names.
#[derive(Clone, Copy, Debug)]
pub enum BookmarkMotion {
    ForwardOnly,
    Anywhere,
}

fn move_bookmark(
    repo: &Path,
    (name, revision): (&str, &str),
    motion: BookmarkMotion,
) -> Result<(), JjError> {
    let repo = Repo::open(repo)?;
    let target = repo.commit(revision)?;
    guard_bookmark_motion(repo.repo.as_ref(), (name, target.id()), motion)?;
    let mut tx = repo.repo.start_transaction();
    tx.repo_mut()
        .set_local_bookmark_target(JjRefName::new(name), RefTarget::normal(target.id().clone()));
    let operation = format!(
        "knives: point {name} at {}",
        short_id(&target.id().to_string())
    );
    commit_mutation(&repo, tx, &operation)
}

/// Refuse a [`BookmarkMotion::ForwardOnly`] move that is backwards or sideways.
///
/// Takes the repo abstraction rather than [`Repo`] so a publish can ask the
/// question inside its own transaction, whose index is the only one that
/// contains the just-written commit.
fn guard_bookmark_motion(
    repo: &dyn jj_lib::repo::Repo,
    (name, target): (&str, &JjCommitId),
    motion: BookmarkMotion,
) -> Result<(), JjError> {
    if matches!(motion, BookmarkMotion::ForwardOnly)
        && let Some(current) = repo
            .view()
            .get_local_bookmark(JjRefName::new(name))
            .as_normal()
        && !repo
            .index()
            .is_ancestor(current, target)
            .map_err(|error| JjError::Revision {
                revision: target.to_string(),
                detail: error.to_string(),
            })?
    {
        return Err(JjError::Revision {
            revision: name.to_owned(),
            detail: "refusing to move the bookmark backwards or sideways".to_owned(),
        });
    }
    Ok(())
}

/// Commits matching a revset, resolved through jj porcelain.
///
/// Exists for queries jj-lib makes hard (glob descriptions, `empty()`,
/// ancestry set arithmetic) and for callers that need "no matches" as an
/// empty answer rather than an error: `jj log` on an EMPTY REVSET prints
/// nothing and exits zero (verified with `none()`). Note that naming a hidden
/// commit id in a revset resurrects it into the resolution (even through
/// `all() & <id>`, verified); callers asking about visibility must list a
/// visibility-scoped revset and test membership themselves.
pub fn commits_matching(repo: &Path, revset: &str) -> Result<Vec<CommitId>, JjError> {
    let repo_path = path(repo);
    let output = command(
        "jj",
        [
            "--repository",
            &repo_path,
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "-r",
            revset,
            "-T",
            "commit_id ++ \"\\n\"",
        ],
    )?;
    Ok(output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| CommitId::new(line.trim()))
        .collect())
}

/// Names reaped and names whose abandon was refused, from one operation.
#[derive(Debug)]
pub struct ReapWrite {
    pub abandoned: Vec<String>,
    pub refused: Vec<(String, JjError)>,
}

/// Forget every ref of each name and abandon its commits, as ONE operation.
///
/// Forgetting first releases the pins the names themselves hold: a superseded
/// cut's own re-materialized `@origin` ref keeps it immutable, and forgetting
/// (unlike deleting) erases local knowledge only — nothing changes on any
/// remote. A commit still immutable AFTER its refs are forgotten (an untracked
/// remote pin someone else pushed, a trunk, a tag) refuses its abandon and is
/// reported without stopping later names — the porcelain forget/abandon
/// sequence this replaces behaved the same way, in two operations per name.
pub fn forget_and_abandon(
    repo: &Path,
    entries: &[(String, Vec<CommitId>)],
    operation: &str,
) -> Result<ReapWrite, JjError> {
    let repo = Repo::open(repo)?;
    let mut tx = repo.repo.start_transaction();
    let mut outcome = ReapWrite {
        abandoned: Vec::new(),
        refused: Vec::new(),
    };
    for (name, targets) in entries {
        forget_refs(tx.repo_mut(), name);
        let commits: Vec<jj_lib::commit::Commit> = targets
            .iter()
            .map(|target| repo.commit(target.as_str()))
            .collect::<Result<_, _>>()?;
        // Gate against the transaction's view: the refs just forgotten no
        // longer pin, exactly as they no longer pinned the porcelain abandon.
        match assert_mutable(tx.repo(), &commits) {
            Ok(()) => {
                for commit in &commits {
                    tx.repo_mut().record_abandoned_commit(commit);
                }
                outcome.abandoned.push(name.clone());
            }
            Err(error) => outcome.refused.push((name.clone(), error)),
        }
    }
    commit_mutation(&repo, tx, operation)?;
    Ok(outcome)
}

/// Forget a bookmark and its remote-tracking refs in the transaction's view:
/// the same view edits `jj bookmark forget --include-remotes` makes. Erases
/// local knowledge only; nothing is deleted on any remote, and a later fetch
/// re-materializes whatever still exists there.
fn forget_refs(mut_repo: &mut MutableRepo, name: &str) {
    let ref_name = JjRefName::new(name);
    let remotes: Vec<String> = mut_repo
        .view()
        .bookmarks()
        .filter(|(bookmark, _)| *bookmark == ref_name)
        .flat_map(|(_, targets)| {
            targets
                .remote_refs
                .into_iter()
                .map(|(remote, _)| remote.as_str().to_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    mut_repo.set_local_bookmark_target(ref_name, RefTarget::absent());
    for remote in &remotes {
        mut_repo.set_remote_bookmark(
            ref_name.to_remote_symbol(JjRemoteName::new(remote)),
            RemoteRef::absent(),
        );
    }
}

/// Abandon commits by explicit id, as ONE described operation.
///
/// Immutable commits refuse, as they do under `jj abandon`. Descendants are
/// rebased onto the abandoned commits' parents in the same operation, so ids
/// of later targets never go stale (the old one-at-a-time porcelain lesson).
/// An empty slice is a no-op.
pub fn abandon_commits(repo: &Path, commits: &[CommitId], operation: &str) -> Result<(), JjError> {
    if commits.is_empty() {
        return Ok(());
    }
    let repo = Repo::open(repo)?;
    let targets: Vec<jj_lib::commit::Commit> = commits
        .iter()
        .map(|commit| repo.commit(commit.as_str()))
        .collect::<Result<_, _>>()?;
    assert_mutable(repo.repo.as_ref(), &targets)?;
    let mut tx = repo.repo.start_transaction();
    for target in &targets {
        tx.repo_mut().record_abandoned_commit(target);
    }
    commit_mutation(&repo, tx, operation)
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

#[cfg(test)]
mod tests {
    use super::ls_remote_args;

    #[test]
    fn ls_remote_always_terminates_option_parsing_before_the_remote_url() {
        assert_eq!(
            ls_remote_args("-looks-like-an-option", &["refs/heads/release/*"]),
            vec![
                "ls-remote",
                "--",
                "-looks-like-an-option",
                "refs/heads/release/*"
            ]
        );
    }
}
