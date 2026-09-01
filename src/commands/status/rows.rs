use super::{
    BTreeMap, BookmarkRef, BookmarkTips, BranchName, BranchRow, BranchState, BranchTarget,
    ChecksSummary, CommitId, Finding, FindingKind, JjError, LandedVerdict, LastNotch, Notch,
    OriginRelation, PriorPull, PullCell, PullDetails, PullIndex, PullRequest, PullSummary,
    PushRelation, ReleaseScheme, Repo, RepoEntry, RepoName, Report, Store, Subject, fmt,
    is_release_name, pull_number_from_bookmark, short,
};

use super::phases::ProbeInput;

/// The pull request a bookmark refers to, by branch name or by fetched-head number.
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
                    state: pull.state.to_lowercase(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Branches we maintain apart from the configured trunk, and fetched pull request heads skipped.
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

fn stated_pull_for(
    target: &BranchTarget,
    store: &Store,
    snapshot: Option<&crate::snapshot::CompletedSnapshot<'_>>,
) -> Option<PullCell> {
    store.tracked_pull(target).map(|number| {
        let pull = snapshot.and_then(|snapshot| snapshot.fact(number));
        PullCell {
            number,
            state: pull.map_or_else(
                || "unknown".to_owned(),
                |fact| fact.pull.state.to_lowercase(),
            ),
            draft: pull.is_some_and(|fact| fact.pull.is_draft),
            stated: Some(true),
            prior: Vec::new(),
        }
    })
}

fn pull_cell(
    inferred: Option<&PullRequest>,
    stated: Option<PullCell>,
    mut prior: Vec<PriorPull>,
) -> Option<PullCell> {
    let mut cell = inferred.map(|pull| PullCell {
        number: pull.number,
        state: pull.state.to_lowercase(),
        draft: pull.is_draft,
        stated: None,
        prior: Vec::new(),
    });
    if let Some(stated) = stated {
        if let Some(primary) = &mut cell {
            if primary.number == stated.number {
                primary.stated = Some(true);
            } else {
                prior.push(PriorPull {
                    number: stated.number,
                    state: stated.state,
                });
            }
        } else {
            cell = Some(stated);
        }
    }
    cell.map(|mut cell| {
        cell.prior = prior;
        cell
    })
}

/// Whether the newest review predates the branch head, when there was a review to compare.
fn review_predates_head_from(
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
fn checks_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<ChecksSummary> {
    let pull_request = pull_request?;
    if !pull_request.is_open() {
        return None;
    }
    details?.checks.clone()
}

fn review_cell(pull: Option<&PullRequest>) -> Option<String> {
    let pull = pull.filter(|pull| pull.is_open())?;
    Some(if pull.review_decision.is_empty() {
        "no-review".to_owned()
    } else {
        pull.review_decision.to_lowercase().replace('_', "-")
    })
}

fn checks_cell(pull: Option<&PullRequest>, checks: Option<&ChecksSummary>) -> Option<String> {
    pull.filter(|pull| pull.is_open())?;
    let checks = checks?;
    Some(if checks.failing() {
        "failing".to_owned()
    } else if !checks.ran() {
        "none-ran".to_owned()
    } else if checks.pending() {
        "pending".to_owned()
    } else {
        "ok".to_owned()
    })
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "one boolean per ordered P4 state makes precedence explicit and testable"
)]
#[derive(Default, Clone, Copy)]
struct BranchStateInput {
    fork_only: bool,
    divergent: bool,
    landed: bool,
    conflicted: bool,
    checks_failing: bool,
    changes_requested: bool,
    approved: bool,
    draft: bool,
    awaiting_review: bool,
    merged: bool,
    closed: bool,
    no_pr: bool,
}

#[derive(Clone, Copy)]
struct StateInput<'a> {
    fork_only: bool,
    divergent: bool,
    landed: Option<LandedVerdict>,
    pull: Option<&'a PullRequest>,
    checks: Option<&'a ChecksSummary>,
    forge_answered: bool,
    pr: Option<&'a PullCell>,
}

/// Applies the reported branch-state taxonomy in its declaration order.
const fn branch_state(input: BranchStateInput) -> BranchState {
    let BranchStateInput {
        fork_only,
        divergent,
        landed,
        conflicted,
        checks_failing,
        changes_requested,
        approved,
        draft,
        awaiting_review,
        merged,
        closed,
        no_pr,
    } = input;
    if fork_only {
        BranchState::ForkOnly
    } else if divergent {
        BranchState::Divergent
    } else if landed {
        BranchState::Landed
    } else if conflicted {
        BranchState::Conflicted
    } else if checks_failing {
        BranchState::ChecksFailing
    } else if changes_requested {
        BranchState::ChangesRequested
    } else if approved {
        BranchState::Approved
    } else if draft {
        BranchState::Draft
    } else if awaiting_review {
        BranchState::AwaitingReview
    } else if merged {
        BranchState::Merged
    } else if closed {
        BranchState::Closed
    } else if no_pr {
        BranchState::NoPr
    } else {
        BranchState::Unknown
    }
}

fn state_for(input: StateInput<'_>) -> BranchState {
    let StateInput {
        fork_only,
        divergent,
        landed,
        pull,
        checks,
        forge_answered,
        pr,
    } = input;
    let is_open = pull.is_some_and(PullRequest::is_open);
    let review = pull
        .filter(|pull| pull.is_open())
        .map(|pull| pull.review_decision.as_str());
    branch_state(BranchStateInput {
        fork_only,
        divergent,
        landed: landed == Some(LandedVerdict::InTrunk),
        conflicted: is_open && pull.is_some_and(PullRequest::conflicting),
        checks_failing: is_open && checks.is_some_and(ChecksSummary::failing),
        changes_requested: review
            .is_some_and(|review| review.eq_ignore_ascii_case("CHANGES_REQUESTED")),
        approved: review.is_some_and(|review| review.eq_ignore_ascii_case("APPROVED")),
        draft: is_open && pull.is_some_and(|pull| pull.is_draft),
        awaiting_review: is_open,
        merged: pull.is_some_and(|pull| pull.state.eq_ignore_ascii_case("MERGED")),
        closed: pull.is_some_and(|pull| pull.state.eq_ignore_ascii_case("CLOSED")),
        no_pr: forge_answered && pr.is_none(),
    })
}

fn flags_for(pull: Option<&PullRequest>, review_predates_head: Option<bool>) -> Vec<String> {
    let mut flags = Vec::new();
    if pull.is_some_and(|pull| pull.merge_state_status.eq_ignore_ascii_case("BEHIND")) {
        flags.push("behind-base".to_owned());
    }
    if review_predates_head == Some(true) {
        flags.push("review-stale".to_owned());
    }
    flags
}

fn push_for(
    origin_tip: Option<&CommitId>,
    relation: Option<PushRelation>,
) -> (Option<PushRelation>, Option<String>) {
    match (origin_tip, relation) {
        (None, _) => (Some(PushRelation::Unpushed), None),
        (
            Some(origin),
            Some(PushRelation::Behind | PushRelation::Diverged | PushRelation::Unresolved),
        ) => (relation, Some(short(origin.as_str()))),
        (Some(_), Some(PushRelation::Unpushed | PushRelation::UnpushedCommits)) => (relation, None),
        (Some(_), None) => (None, None),
    }
}

#[derive(Clone, Copy)]
struct PullFindingInput<'a> {
    branch: &'a BranchName,
    pull: Option<&'a PullRequest>,
    checks: Option<&'a ChecksSummary>,
    review_predates_head: Option<bool>,
    expected_base: &'a str,
}

fn add_pull_findings(findings: &mut Vec<Finding>, input: PullFindingInput<'_>) {
    let PullFindingInput {
        branch,
        pull,
        checks,
        review_predates_head,
        expected_base,
    } = input;
    let Some(pull) = pull else {
        return;
    };
    if pull.conflicting() {
        findings.push(Finding::new(
            FindingKind::Unmergeable,
            Subject::PullRequest(pull.number),
            format!("#{} cannot be merged as it stands", pull.number),
        ));
    }
    if pull.is_open() && checks.is_some_and(ChecksSummary::failing) {
        findings.push(Finding::new(
            FindingKind::ChecksFailing,
            Subject::PullRequest(pull.number),
            format!("#{} has failing checks", pull.number),
        ));
    }
    if review_predates_head == Some(true) {
        findings.push(Finding::new(
            FindingKind::StaleReview,
            Subject::PullRequest(pull.number),
            format!(
                "the newest review on #{} predates the newest commit on {branch}",
                pull.number
            ),
        ));
    }
    if pull.is_open() && !pull.base_ref_name.is_empty() && pull.base_ref_name != expected_base {
        findings.push(Finding::new(
            FindingKind::WrongBase,
            Subject::PullRequest(pull.number),
            format!(
                "#{} targets {}, not {expected_base}",
                pull.number, pull.base_ref_name
            ),
        ));
    }
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
    pub(super) snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    pub(super) index: &'a PullIndex,
    pub(super) notches: &'a [Notch],
    pub(super) expected_base: &'a str,
}

/// Rows for divergent local bookmarks.
pub(super) fn divergent_rows(
    input: &DivergentInput<'_, '_>,
    findings: &mut Vec<Finding>,
) -> Vec<BranchRow> {
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
            let pull = fact.map(|fact| &fact.pull);
            let details = fact.map(|fact| &fact.details);
            let checks = checks_from(details, pull);
            let review_predates_head = review_predates_head_from(details, pull);
            add_pull_findings(
                findings,
                PullFindingInput {
                    branch,
                    pull,
                    checks: checks.as_ref(),
                    review_predates_head,
                    expected_base: input.expected_base,
                },
            );
            let raw_origin = input.tips.get(&BookmarkRef::Remote {
                branch: branch.clone(),
                remote: crate::ids::RemoteName::new("origin"),
            });
            let (push, origin_tip) = raw_origin
                .map_or((Some(PushRelation::Unpushed), None), |origin| {
                    (Some(PushRelation::Unresolved), Some(short(origin.as_str())))
                });
            let fork_only = input.store.is_fork_only(&target);
            let pr = pull_cell(
                pull,
                stated_pull_for(&target, input.store, input.snapshot),
                prior_pulls_for(branch, &input.index.prior),
            );
            BranchRow {
                name: branch.clone(),
                state: state_for(StateInput {
                    fork_only,
                    divergent: true,
                    landed: None,
                    pull,
                    checks: checks.as_ref(),
                    forge_answered: input.snapshot.is_some(),
                    pr: pr.as_ref(),
                }),
                tip: None,
                push,
                origin_tip,
                pr,
                review: review_cell(pull),
                checks: checks_cell(pull, checks.as_ref()),
                landed: None,
                flags: flags_for(pull, review_predates_head),
                claim: None,
                last_seen: None,
                seen: None,
                workspace: None,
                notch: LastNotch::of(
                    input
                        .notches
                        .iter()
                        .filter(|notch| notch.subject.as_deref() == Some(branch.as_str())),
                ),
            }
        })
        .collect()
}

pub(super) fn record_origin_relation<E: fmt::Display>(
    report: &mut Report,
    branch: &BranchName,
    relation: Result<Option<OriginRelation>, E>,
) -> Option<PushRelation> {
    match relation {
        Ok(relation) => relation.map(|relation| match relation {
            OriginRelation::Ahead => PushRelation::UnpushedCommits,
            OriginRelation::Behind => PushRelation::Behind,
            OriginRelation::Diverged => PushRelation::Diverged,
        }),
        Err(error) => {
            report.problems.push(format!(
                "cannot tell how {branch} relates to origin: {error}"
            ));
            Some(PushRelation::Unresolved)
        }
    }
}

/// Everything the maintained-branch row loop needs after the two concurrent phases end.
pub(super) struct RowInput<'a, 'snapshot> {
    pub(super) name: &'a RepoName,
    pub(super) store: &'a Store,
    pub(super) probe_inputs: Vec<ProbeInput>,
    pub(super) verdicts: Vec<Result<Option<LandedVerdict>, JjError>>,
    pub(super) origin_relations: Vec<Result<Option<OriginRelation>, String>>,
    pub(super) index: &'a PullIndex,
    pub(super) snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    pub(super) notches: &'a [Notch],
    pub(super) expected_base: &'a str,
}

/// The branch rows, and the branches whose landed state could not be judged.
pub(super) fn branch_rows(
    row_input: RowInput<'_, '_>,
    report: &mut Report,
    findings: &mut Vec<Finding>,
) -> anyhow::Result<Vec<String>> {
    let mut unjudged = Vec::new();
    for ((verdict, probe_input), relation) in row_input
        .verdicts
        .into_iter()
        .zip(row_input.probe_inputs)
        .zip(row_input.origin_relations)
    {
        let branch = probe_input.branch;
        let tip = probe_input.tip;
        let raw_origin = probe_input.origin_tip;
        let landed = verdict?;
        if landed == Some(LandedVerdict::Unjudged) {
            unjudged.push(branch.to_string());
        }
        let fact = pull_summary_for(&branch, &row_input.index.by_branch).and_then(|summary| {
            row_input
                .snapshot
                .and_then(|snapshot| snapshot.fact(summary.number))
        });
        let pull = fact.map(|fact| &fact.pull);
        let details = fact.map(|fact| &fact.details);
        let checks = checks_from(details, pull);
        let review_predates_head = review_predates_head_from(details, pull);
        add_pull_findings(
            findings,
            PullFindingInput {
                branch: &branch,
                pull,
                checks: checks.as_ref(),
                review_predates_head,
                expected_base: row_input.expected_base,
            },
        );
        let relation = record_origin_relation(report, &branch, relation);
        let (push, origin_tip) = push_for(raw_origin.as_ref(), relation);
        let target = BranchTarget::new(row_input.name.clone(), branch.clone());
        let fork_only = row_input.store.is_fork_only(&target);
        let pr = pull_cell(
            pull,
            stated_pull_for(&target, row_input.store, row_input.snapshot),
            prior_pulls_for(&branch, &row_input.index.prior),
        );
        report.branches.push(BranchRow {
            name: branch.clone(),
            state: state_for(StateInput {
                fork_only,
                divergent: false,
                landed,
                pull,
                checks: checks.as_ref(),
                forge_answered: row_input.snapshot.is_some(),
                pr: pr.as_ref(),
            }),
            tip: Some(short(tip.as_str())),
            push,
            origin_tip,
            pr,
            review: review_cell(pull),
            checks: checks_cell(pull, checks.as_ref()),
            landed,
            flags: flags_for(pull, review_predates_head),
            claim: None,
            last_seen: None,
            seen: None,
            workspace: None,
            notch: LastNotch::of(
                row_input
                    .notches
                    .iter()
                    .filter(|notch| notch.subject.as_deref() == Some(branch.as_str())),
            ),
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
    use super::super::test_fixtures::{local, tips};
    use super::*;

    fn input() -> BranchStateInput {
        BranchStateInput::default()
    }
    fn state_input<'a>(pull: Option<&'a PullRequest>, pr: Option<&'a PullCell>) -> StateInput<'a> {
        StateInput {
            fork_only: false,
            divergent: false,
            landed: None,
            pull,
            checks: None,
            forge_answered: true,
            pr,
        }
    }

    #[test]
    fn branch_state_precedence_is_first_match_wins() {
        let mut case = input();
        case.fork_only = true;
        case.divergent = true;
        assert_eq!(branch_state(case), BranchState::ForkOnly);

        let mut case = input();
        case.divergent = true;
        case.landed = true;
        assert_eq!(branch_state(case), BranchState::Divergent);

        let mut case = input();
        case.landed = true;
        case.conflicted = true;
        assert_eq!(branch_state(case), BranchState::Landed);

        let mut case = input();
        case.conflicted = true;
        case.checks_failing = true;
        assert_eq!(branch_state(case), BranchState::Conflicted);

        let mut case = input();
        case.checks_failing = true;
        case.changes_requested = true;
        assert_eq!(branch_state(case), BranchState::ChecksFailing);

        let mut case = input();
        case.changes_requested = true;
        case.approved = true;
        assert_eq!(branch_state(case), BranchState::ChangesRequested);

        let mut case = input();
        case.approved = true;
        case.draft = true;
        assert_eq!(branch_state(case), BranchState::Approved);

        let mut case = input();
        case.draft = true;
        case.awaiting_review = true;
        assert_eq!(branch_state(case), BranchState::Draft);

        let mut case = input();
        case.awaiting_review = true;
        case.merged = true;
        assert_eq!(branch_state(case), BranchState::AwaitingReview);

        let mut case = input();
        case.merged = true;
        case.closed = true;
        assert_eq!(branch_state(case), BranchState::Merged);

        let mut case = input();
        case.closed = true;
        case.no_pr = true;
        assert_eq!(branch_state(case), BranchState::Closed);

        let mut case = input();
        case.no_pr = true;
        assert_eq!(branch_state(case), BranchState::NoPr);

        assert_eq!(branch_state(input()), BranchState::Unknown);
    }

    #[test]
    fn terminal_pull_states_outrank_review_decisions_and_draft() {
        let approved_closed = PullRequest {
            state: "CLOSED".to_owned(),
            review_decision: "APPROVED".to_owned(),
            ..PullRequest::default()
        };
        assert_eq!(
            state_for(state_input(Some(&approved_closed), None)),
            BranchState::Closed
        );

        let closed_draft = PullRequest {
            state: "CLOSED".to_owned(),
            is_draft: true,
            ..PullRequest::default()
        };
        assert_eq!(
            state_for(state_input(Some(&closed_draft), None)),
            BranchState::Closed
        );
    }

    #[test]
    fn a_tracked_unavailable_pull_is_not_no_pr() {
        let tracked_unavailable = Some(PullCell {
            number: 42,
            state: "unknown".to_owned(),
            draft: false,
            stated: Some(true),
            prior: Vec::new(),
        });
        assert_eq!(
            state_for(state_input(None, tracked_unavailable.as_ref())),
            BranchState::Unknown
        );
    }

    #[test]
    fn the_trunk_exclusion_follows_the_repo_entry_not_the_name_main() {
        let map = tips(&[
            (local("dev"), "aaa"),
            (local("main"), "bbb"),
            (local("feat/alpha"), "ccc"),
        ]);
        let (branches, _) = maintained_branches(&map, "dev", &ReleaseScheme::Dated);
        let names: Vec<String> = branches
            .iter()
            .map(|(branch, _)| branch.to_string())
            .collect();
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

        assert_eq!(relation, Some(PushRelation::Unresolved));
        assert!(
            report.problems.iter().any(|problem| {
                problem.contains("cannot tell how feat/alpha relates to origin")
            })
        );
    }
}
