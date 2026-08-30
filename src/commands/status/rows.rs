use super::*;

use super::phases::ProbeInput;
/// The pull request a bookmark refers to, by branch name or by fetched-head number.
///
/// A `pr-<n>` bookmark is a fetched pull request head: its name is the number, not
/// the branch the pull request came from, so matching on name alone never found one.
pub(super) fn pull_summary_for<'a>(
    branch: &BranchName,
    index: &'a BTreeMap<BranchName, PullSummary>,
) -> Option<&'a PullSummary> {
    index.get(branch).or_else(|| {
        let number = pull_number_from_bookmark(branch.as_str())?;
        index.values().find(|pull| pull.number == number)
    })
}

/// The shadowed pull requests for one branch, compacted for its row.
pub(super) fn prior_pulls_for(
    branch: &BranchName,
    prior: &BTreeMap<BranchName, Vec<PullSummary>>,
) -> Vec<PriorPull> {
    prior
        .get(branch)
        .map(|shadowed| {
            shadowed
                .iter()
                .map(|pull| PriorPull {
                    number: pull.number,
                    state: pull.state.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}
/// Branches we maintain apart from the configured trunk, and fetched pull request heads skipped.
///
/// A `pr-<n>` bookmark is not a branch of ours: it is a pull request head this tool
/// fetched so a release could carry it. Treating fetch artifacts as our work is most of
/// why this report was unreadable — on one repository they were 16 of 28 rows and 10 of
/// 24 findings, every one of the latter advising us to drop a branch that was never
/// ours. They also each cost a landed probe, which is most of the runtime.
pub(super) fn maintained_branches(
    tips: &BookmarkTips,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> (Vec<(BranchName, CommitId)>, usize) {
    let mut fetched_heads = 0_usize;
    let branches = tips
        .iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch)
                if !is_release_name(branch, scheme) && branch.as_str() != trunk =>
            {
                if pull_number_from_bookmark(branch.as_str()).is_some() {
                    fetched_heads += 1;
                    return None;
                }
                Some((branch.clone(), commit.clone()))
            }
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        })
        .collect();
    (branches, fetched_heads)
}
/// The pull request stated for a branch, with the current snapshot's answer.
pub(super) fn stated_pull_for(
    target: &BranchTarget,
    store: &Store,
    snapshot: Option<&crate::snapshot::ForgeSnapshot<'_>>,
) -> Option<StatedPull> {
    store.tracked_pull(target).map(|number| StatedPull {
        state: snapshot
            .and_then(|snapshot| snapshot.fact(number))
            .map_or_else(|| "unknown".to_owned(), |fact| fact.pull.state.clone()),
        number,
    })
}
/// Whether the newest review predates the branch head, when there was a review to
/// compare.
///
/// Gated as the per-pull-request call was: an empty review decision means the
/// forge recorded no review, and `None` must never render as "current".
pub(super) fn review_stale_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<bool> {
    let pull_request = pull_request?;
    if pull_request.review_decision.is_empty() {
        return None;
    }
    details?.review_predates_head
}

/// What the forge's checks say, for an open pull request that was consulted.
///
/// Settled pull requests are not asked about and not reported on: a closed one's
/// recorded rollup is obsolete the moment it closes.
pub(super) fn checks_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<ChecksSummary> {
    let pull_request = pull_request?;
    if !pull_request.is_open() {
        return None;
    }
    details?.checks.clone()
}

/// The locally divergent branches that need rows but have no tip to probe.
pub(super) fn divergent_branch_names(
    repo: &Repo,
    entry: &RepoEntry,
) -> anyhow::Result<Vec<BranchName>> {
    let scheme = entry.release_scheme();
    Ok(repo
        .conflicted_bookmarks()?
        .into_iter()
        .filter_map(|(reference, _)| {
            let BookmarkRef::Local(branch) = reference else {
                return None;
            };
            (!is_release_name(&branch, &scheme)
                && branch.as_str() != entry.trunk()
                && pull_number_from_bookmark(branch.as_str()).is_none())
            .then_some(branch)
        })
        .collect())
}

pub(super) struct DivergentInput<'a, 'snapshot> {
    pub(super) branches: &'a [BranchName],
    pub(super) tips: &'a BookmarkTips,
    pub(super) name: &'a RepoName,
    pub(super) store: &'a Store,
    pub(super) snapshot: Option<&'a crate::snapshot::ForgeSnapshot<'snapshot>>,
    pub(super) index: &'a PullIndex,
    pub(super) notches: &'a [Notch],
}

/// Rows for divergent local bookmarks.
///
/// `bookmark_tips` cannot report these: a conflicted target has no single commit, so
/// jj-lib yields nothing for it. Without them these branches were absent from the
/// listing entirely, and a branch with no row got no pull request association either, so
/// its pull request read as nonexistent until somebody happened to resolve the
/// divergence. Proven by before-and-after on #228.
pub(super) fn divergent_rows(input: &DivergentInput<'_, '_>) -> Vec<BranchRow> {
    input
        .branches
        .iter()
        .map(|branch| {
            let target = BranchTarget::new(input.name.clone(), branch.clone());
            let fact = pull_summary_for(branch, &input.index.by_branch).and_then(|summary| {
                input
                    .snapshot
                    .and_then(|snapshot| snapshot.fact(summary.number))
            });
            let (pull_request, review_stale, checks) = fact.map_or_else(
                || (None, None, None),
                |fact| {
                    (
                        Some(fact.pull.clone()),
                        review_stale_from(Some(&fact.details), Some(&fact.pull)),
                        checks_from(Some(&fact.details), Some(&fact.pull)),
                    )
                },
            );
            BranchRow {
                fork_only: input.store.is_fork_only(&target),
                stated_pull: stated_pull_for(&target, input.store, input.snapshot),
                pull_request,
                review_stale,
                checks,
                prior_pulls: prior_pulls_for(branch, &input.index.prior),
                origin_tip: input
                    .tips
                    .get(&BookmarkRef::Remote {
                        branch: branch.clone(),
                        remote: crate::ids::RemoteName::new("origin"),
                    })
                    .cloned(),
                last_notch: newest_for(input.notches, branch.as_str()).map(LastNotch::of),
                // Nothing to replay: a divergent bookmark has no single commit to probe.
                ..BranchRow::bare(branch.clone(), None)
            }
        })
        .collect()
}
pub(super) fn record_origin_relation<E: fmt::Display>(
    report: &mut Report,
    branch: &BranchName,
    relation: Result<Option<OriginRelation>, E>,
) -> Option<OriginRelation> {
    match relation {
        Ok(relation) => relation,
        Err(error) => {
            report.problems.push(format!(
                "cannot tell how {branch} relates to origin: {error}"
            ));
            None
        }
    }
}
/// Everything the maintained-branch row loop needs after the two concurrent phases end.
pub(super) struct RowInput<'a, 'snapshot> {
    pub(super) name: &'a RepoName,
    pub(super) store: &'a Store,
    pub(super) probe_inputs: Vec<ProbeInput>,
    pub(super) index: &'a PullIndex,
    pub(super) snapshot: Option<&'a crate::snapshot::ForgeSnapshot<'snapshot>>,
    pub(super) notches: &'a [Notch],
}

/// The branch rows, and the branches whose landed state could not be judged.
pub(super) fn branch_rows(
    row_input: RowInput<'_, '_>,
    verdicts: Vec<Result<Option<LandedVerdict>, JjError>>,
    origin_relations: Vec<Result<Option<OriginRelation>, String>>,
    report: &mut Report,
) -> anyhow::Result<Vec<String>> {
    let mut unjudged = Vec::new();
    for ((verdict, probe_input), relation) in verdicts
        .into_iter()
        .zip(row_input.probe_inputs)
        .zip(origin_relations)
    {
        let branch = probe_input.branch;
        let tip = probe_input.tip;
        let origin_tip = probe_input.origin_tip;
        // Propagated in branch order, so a probe failure reports the same branch
        // and the same message it did when probes ran one at a time.
        let landed = verdict?;
        if landed == Some(LandedVerdict::Unjudged) {
            unjudged.push(branch.to_string());
        }
        let fact = pull_summary_for(&branch, &row_input.index.by_branch).and_then(|summary| {
            row_input
                .snapshot
                .and_then(|snapshot| snapshot.fact(summary.number))
        });
        let (pull_request, review_stale, checks) = fact.map_or_else(
            || (None, None, None),
            |fact| {
                (
                    Some(fact.pull.clone()),
                    review_stale_from(Some(&fact.details), Some(&fact.pull)),
                    checks_from(Some(&fact.details), Some(&fact.pull)),
                )
            },
        );
        let origin_relation = record_origin_relation(report, &branch, relation);
        let target = BranchTarget::new(row_input.name.clone(), branch.clone());
        let last_notch = newest_for(row_input.notches, branch.as_str()).map(LastNotch::of);
        report.branches.push(BranchRow {
            prior_pulls: prior_pulls_for(&branch, &row_input.index.prior),
            name: branch,
            tip: Some(tip),
            origin_tip,
            origin_relation,
            pull_request,
            landed,
            review_stale,
            checks,
            fork_only: row_input.store.is_fork_only(&target),
            stated_pull: stated_pull_for(&target, row_input.store, row_input.snapshot),
            last_notch,
        });
    }
    Ok(unjudged)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use super::super::test_fixtures::{local, tips};
    #[test]
    fn the_trunk_exclusion_follows_the_repo_entry_not_the_name_main() {
        // Given: a repo whose upstream trunk is dev, carrying a branch named main
        let map = tips(&[
            (local("dev"), "aaa"),
            (local("main"), "bbb"),
            (local("feat/alpha"), "ccc"),
        ]);
        // When: maintained branches are collected with dev as the trunk
        let (branches, _) = maintained_branches(&map, "dev", &ReleaseScheme::Dated);
        let names: Vec<String> = branches
            .iter()
            .map(|(branch, _)| branch.to_string())
            .collect();
        // Then: dev is excluded as the trunk, and a branch that merely shares the
        // name main is ours to report
        assert!(!names.contains(&"dev".to_owned()), "was: {names:?}");
        assert!(names.contains(&"main".to_owned()), "was: {names:?}");
        assert!(names.contains(&"feat/alpha".to_owned()));
    }
    #[test]
    fn an_unresolved_origin_relation_records_its_branch() {
        let mut report = Report::default();
        let branch = BranchName::new("feat/alpha");
        let relation = record_origin_relation(
            &mut report,
            &branch,
            Err(JjError::Revision {
                revision: "0000000000000000000000000000000000000000".to_owned(),
                detail: "missing".to_owned(),
            }),
        );

        assert_eq!(relation, None);
        assert!(
            report.problems.iter().any(|problem| {
                problem.contains("cannot tell how feat/alpha relates to origin")
            })
        );
    }
}
