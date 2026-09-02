//! Landed verdicts the forge can settle: a merged pull request whose landing
//! commit the upstream trunk contains.
//!
//! The replay probe cannot recognise a squash merge. Replaying a branch onto a
//! trunk that already carries its squash always conflicts — the branch's own
//! squash is in the way — so every squash-merged pull request read
//! `conflicts-with-trunk`, indistinguishable from one the maintainer declined;
//! a divergent bookmark is never probed at all and had no verdict.
//!
//! The forge records where a pull request landed (`mergeCommit`), and the local
//! upstream view says whether the trunk reaches that commit. When it does, and
//! the local branch holds nothing past what the pull request merged, the branch
//! is in the trunk by evidence rather than by replay.

use std::collections::BTreeMap;

use crate::commands::status::{BranchRow, BranchState, Report};
use crate::detect::{BookmarkTips, LandedVerdict};
use crate::forge::{PullIndex, PullSummary};
use crate::ids::{BookmarkRef, BranchName, CommitId};
use crate::jj::Repo;

use super::rows::pull_summary_for;
use super::short;

#[derive(Clone, Copy)]
pub(super) struct MergedLandedInput<'a> {
    pub(super) repo: &'a Repo,
    pub(super) index: &'a PullIndex,
    pub(super) tips: &'a BookmarkTips,
    /// Every commit a divergent bookmark names, keyed by branch: those rows
    /// have no single tip in `tips`.
    pub(super) divergent_tips: &'a BTreeMap<BranchName, Vec<CommitId>>,
    pub(super) trunk_tip: Option<&'a CommitId>,
}

/// What the local branch holds relative to the commit the pull request merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalContent {
    /// Every local tip is the merged head or an ancestor of it.
    WithinMerged,
    /// Some local tip is not within the merged head: commits past it, or a
    /// rewrite of the same work the head does not reach.
    PastMerged,
}

/// `None` when the row's bookmark names no commit at all.
fn local_content(
    repo: &Repo,
    local_tips: &[CommitId],
    merged_head: &CommitId,
) -> Result<Option<LocalContent>, crate::jj::JjError> {
    if local_tips.is_empty() {
        return Ok(None);
    }
    for tip in local_tips {
        if tip != merged_head && !repo.is_ancestor(tip, merged_head)? {
            return Ok(Some(LocalContent::PastMerged));
        }
    }
    Ok(Some(LocalContent::WithinMerged))
}

/// The commits a row's bookmark names: one for a normal branch, several for a
/// divergent one.
fn local_tips(
    row: &BranchRow,
    tips: &BookmarkTips,
    divergent_tips: &BTreeMap<BranchName, Vec<CommitId>>,
) -> Vec<CommitId> {
    if let Some(tip) = tips.get(&BookmarkRef::Local(row.name.clone())) {
        return vec![tip.clone()];
    }
    divergent_tips.get(&row.name).cloned().unwrap_or_default()
}

/// Settle `landed` for rows whose merged pull request the trunk contains.
///
/// Rows already judged `in-trunk` are left alone. A merged pull request whose
/// landing commit or merged head the local repository lacks is noted — `knives
/// sync` fetches them — rather than guessed at. A branch holding a tip the
/// merged head does not reach keeps its replay verdict, with a note saying why:
/// the pull request landed, the branch is somewhere else. An ancestry question
/// jj cannot answer is an error, not a verdict.
pub(super) fn settle_merged_landed(
    report: &mut Report,
    input: MergedLandedInput<'_>,
) -> anyhow::Result<()> {
    let MergedLandedInput {
        repo,
        index,
        tips,
        divergent_tips,
        trunk_tip,
    } = input;
    let Some(trunk_tip) = trunk_tip else {
        return Ok(());
    };
    let mut notes = Vec::new();
    for row in &mut report.branches {
        if row.landed == Some(LandedVerdict::InTrunk) {
            continue;
        }
        let Some(summary) = pull_summary_for(&row.name, &index.by_branch) else {
            continue;
        };
        let Some(landing) = merged_landing(summary) else {
            continue;
        };
        let Ok(landing_commit) = repo.resolve_commit(landing) else {
            notes.push(format!(
                "{}: #{} merged as {}, which the local upstream view does not have; \
                 `knives sync` fetches it",
                row.name,
                summary.number,
                short(landing)
            ));
            continue;
        };
        if !repo.is_ancestor(&landing_commit, trunk_tip)? {
            continue;
        }
        let Ok(merged_head) = repo.resolve_commit(&summary.head_ref_oid) else {
            notes.push(format!(
                "{}: #{} merged its head {}, which the local repository does not have, so \
                 whether the branch holds more than it merged cannot be said; `knives sync` \
                 fetches it",
                row.name,
                summary.number,
                short(&summary.head_ref_oid)
            ));
            continue;
        };
        match local_content(repo, &local_tips(row, tips, divergent_tips), &merged_head)? {
            Some(LocalContent::WithinMerged) => {
                row.landed = Some(LandedVerdict::InTrunk);
                if !matches!(row.state, BranchState::ForkOnly | BranchState::Divergent) {
                    row.state = BranchState::Landed;
                }
            }
            Some(LocalContent::PastMerged) => {
                let verdict = if row.landed.is_some() {
                    "landed is judged by replay"
                } else if row.state == BranchState::Divergent {
                    "landed cannot be judged until the branch has one tip"
                } else {
                    "landed was not probed"
                };
                notes.push(format!(
                    "{}: #{} merged as {} and the trunk has it, but the local branch holds a \
                     tip the merged head {} does not reach; {verdict}",
                    row.name,
                    summary.number,
                    short(landing),
                    short(&summary.head_ref_oid)
                ));
            }
            None => {}
        }
    }
    report.notes.extend(notes);
    Ok(())
}

/// The landing commit of a merged pull request, when the forge recorded one.
fn merged_landing(summary: &PullSummary) -> Option<&str> {
    if !summary.is_merged() {
        return None;
    }
    summary
        .merge_commit
        .as_ref()
        .map(|merge| merge.oid.as_str())
}
