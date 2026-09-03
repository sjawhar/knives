use super::{
    BTreeMap, BookmarkRef, BookmarkTips, BranchName, BranchRow, BranchState, BranchTarget,
    ChecksSummary, CommitId, Finding, FindingKind, JjError, LandedVerdict, LastNotch, Notch,
    OriginRelation, PriorPull, PullCell, PullDetails, PullIndex, PullRequest, PullSummary,
    PushRelation, ReleaseScheme, Repo, RepoEntry, RepoName, Report, Store, Subject, fmt,
    is_release_name, pull_number_from_bookmark,
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
            activity_at: pull.and_then(|fact| fact.newest_comment.clone()),
            prior: Vec::new(),
        }
    })
}

fn pull_cell(
    inferred: Option<&PullRequest>,
    activity_at: Option<&str>,
    stated: Option<PullCell>,
    mut prior: Vec<PriorPull>,
) -> Option<PullCell> {
    let mut cell = inferred.map(|pull| PullCell {
        number: pull.number,
        state: pull.state.to_lowercase(),
        draft: pull.is_draft,
        stated: None,
        activity_at: activity_at.map(str::to_owned),
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

/// The checks column. `action-required` is a workflow the forge is holding for
/// approval rather than one that failed; the row is red either way, but a
/// reader deciding whether to fix code or ask a maintainer needs the difference.
fn checks_cell(pull: Option<&PullRequest>, checks: Option<&ChecksSummary>) -> Option<String> {
    pull.filter(|pull| pull.is_open())?;
    let checks = checks?;
    Some(if checks.has_hard_failure() {
        "failing".to_owned()
    } else if checks.has_action_required() {
        "action-required".to_owned()
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
    } else if draft {
        BranchState::Draft
    } else if approved {
        BranchState::Approved
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
    if is_open && pull.is_some_and(|pull| pull.missing_merge_fields().next().is_some()) {
        return BranchState::Unknown;
    }
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
    if pull.is_some_and(|pull| {
        pull.merge_state_status
            .as_deref()
            .is_some_and(|state| state.eq_ignore_ascii_case("BEHIND"))
    }) {
        flags.push("behind-base".to_owned());
    }
    if review_predates_head == Some(true) {
        flags.push("review-stale".to_owned());
    }
    flags
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingPushRelation {
    UnpushedCommits,
    Behind,
    Diverged,
    Unresolved,
}

fn push_for(
    origin_tip: Option<&CommitId>,
    relation: Option<PendingPushRelation>,
) -> Option<PushRelation> {
    match (origin_tip, relation) {
        (None, _) => Some(PushRelation::Unpushed),
        (Some(_), Some(PendingPushRelation::UnpushedCommits)) => {
            Some(PushRelation::UnpushedCommits)
        }
        (Some(origin), Some(PendingPushRelation::Behind)) => {
            Some(PushRelation::Behind(origin.short().to_owned()))
        }
        (Some(origin), Some(PendingPushRelation::Diverged)) => {
            Some(PushRelation::Diverged(origin.short().to_owned()))
        }
        (Some(origin), Some(PendingPushRelation::Unresolved)) => {
            Some(PushRelation::Unresolved(origin.short().to_owned()))
        }
        (Some(_), None) => None,
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

fn add_pull_findings(
    problems: &mut Vec<String>,
    findings: &mut Vec<Finding>,
    input: PullFindingInput<'_>,
) {
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
    for field in pull.missing_merge_fields() {
        problems.push(format!("#{}: forge did not report {field}", pull.number));
    }
    if pull.conflicting() {
        findings.push(Finding::new(
            FindingKind::Unmergeable,
            Subject::PullRequest(pull.number),
            format!("#{} cannot be merged as it stands", pull.number),
        ));
    }
    if pull.is_open()
        && let Some(checks) = checks
    {
        let failed = checks.hard_failure_names();
        let held = checks.action_required_names();
        // A check that ran and failed is a code problem; a check the forge is
        // holding for action — a workflow awaiting a maintainer's approval, which
        // runs nothing until then — is somebody's call. Both are red; the reader
        // deciding what to do needs the names either way.
        let detail = if !failed.is_empty() {
            Some(format!(
                "#{} has failing checks: {}",
                pull.number,
                failed.join(", ")
            ))
        } else if !held.is_empty() {
            Some(format!(
                "#{} has {} check(s) held for action (an unapproved workflow runs nothing): {}",
                pull.number,
                held.len(),
                held.join(", ")
            ))
        } else {
            None
        };
        if let Some(detail) = detail {
            findings.push(Finding::new(
                FindingKind::ChecksFailing,
                Subject::PullRequest(pull.number),
                detail,
            ));
        }
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
    if pull.is_open()
        && let Some(base) = pull.base_ref_name.as_deref()
        && base != expected_base
    {
        findings.push(Finding::new(
            FindingKind::WrongBase,
            Subject::PullRequest(pull.number),
            format!("#{} targets {base}, not {expected_base}", pull.number),
        ));
    }
}

/// The locally divergent branches that need rows but have no tip to probe,
/// each with every commit its bookmark names.
pub(super) fn divergent_branches(
    repo: &Repo,
    entry: &RepoEntry,
) -> anyhow::Result<BTreeMap<BranchName, Vec<CommitId>>> {
    let scheme = entry.release_scheme();
    Ok(repo
        .conflicted_bookmarks()?
        .into_iter()
        .filter_map(|(reference, commits)| {
            let BookmarkRef::Local(branch) = reference else {
                return None;
            };
            (!is_release_name(&branch, &scheme)
                && branch.as_str() != entry.trunk()
                && pull_number_from_bookmark(branch.as_str()).is_none())
            .then_some((branch, commits))
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

struct RowContext<'a, 'snapshot> {
    name: &'a RepoName,
    store: &'a Store,
    index: &'a PullIndex,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    notches: &'a [Notch],
    expected_base: &'a str,
}

struct RowOutput<'a> {
    report: &'a mut Report,
    findings: &'a mut Vec<Finding>,
}

struct RowFacts<'a> {
    divergent: bool,
    tip: Option<String>,
    landed: Option<LandedVerdict>,
    push: Option<PushRelation>,
    origin_tip: Option<&'a CommitId>,
    origin_relation: Option<Result<Option<OriginRelation>, String>>,
}

fn build_branch_row(
    context: &RowContext<'_, '_>,
    output: &mut RowOutput<'_>,
    branch: &BranchName,
    facts: RowFacts<'_>,
) -> BranchRow {
    let fact = pull_summary_for(branch, &context.index.by_branch).and_then(|summary| {
        context
            .snapshot
            .and_then(|snapshot| snapshot.fact(summary.number))
    });
    let pull = fact.map(|fact| &fact.pull);
    let details = fact.map(|fact| &fact.details);
    let checks = checks_from(details, pull);
    let review_predates_head = review_predates_head_from(details, pull);
    add_pull_findings(
        &mut output.report.problems,
        output.findings,
        PullFindingInput {
            branch,
            pull,
            checks: checks.as_ref(),
            review_predates_head,
            expected_base: context.expected_base,
        },
    );
    let push = if let Some(relation) = facts.origin_relation {
        push_for(
            facts.origin_tip,
            record_origin_relation(output.report, branch, relation),
        )
    } else {
        facts.push
    };
    let target = BranchTarget::new(context.name.clone(), branch.clone());
    let fork_only = context.store.is_fork_only(&target);
    let pr = pull_cell(
        pull,
        fact.and_then(|fact| fact.newest_comment.as_deref()),
        stated_pull_for(&target, context.store, context.snapshot),
        prior_pulls_for(branch, &context.index.prior),
    );
    BranchRow {
        name: branch.clone(),
        state: state_for(StateInput {
            fork_only,
            divergent: facts.divergent,
            landed: facts.landed,
            pull,
            checks: checks.as_ref(),
            forge_answered: context.snapshot.is_some(),
            pr: pr.as_ref(),
        }),
        tip: facts.tip,
        push,
        pr,
        review: review_cell(pull),
        checks: checks_cell(pull, checks.as_ref()),
        landed: facts.landed,
        flags: flags_for(pull, review_predates_head),
        claim: None,
        last_seen: None,
        seen: None,
        workspace: None,
        notch: LastNotch::of(
            context
                .notches
                .iter()
                .filter(|notch| notch.subject.as_deref() == Some(branch.as_str())),
        ),
    }
}

/// Rows for divergent local bookmarks.
pub(super) fn divergent_rows(
    input: &DivergentInput<'_, '_>,
    report: &mut Report,
    findings: &mut Vec<Finding>,
) -> Vec<BranchRow> {
    let context = RowContext {
        name: input.name,
        store: input.store,
        index: input.index,
        snapshot: input.snapshot,
        notches: input.notches,
        expected_base: input.expected_base,
    };
    let mut output = RowOutput { report, findings };
    input
        .branches
        .iter()
        .map(|branch| {
            let raw_origin = input.tips.get(&BookmarkRef::Remote {
                branch: branch.clone(),
                remote: crate::ids::RemoteName::new("origin"),
            });
            build_branch_row(
                &context,
                &mut output,
                branch,
                RowFacts {
                    divergent: true,
                    tip: None,
                    landed: None,
                    push: raw_origin.map_or(Some(PushRelation::Unpushed), |origin| {
                        Some(PushRelation::Unresolved(origin.short().to_owned()))
                    }),
                    origin_tip: None,
                    origin_relation: None,
                },
            )
        })
        .collect()
}

fn record_origin_relation<E: fmt::Display>(
    report: &mut Report,
    branch: &BranchName,
    relation: Result<Option<OriginRelation>, E>,
) -> Option<PendingPushRelation> {
    match relation {
        Ok(relation) => relation.map(|relation| match relation {
            OriginRelation::Ahead => PendingPushRelation::UnpushedCommits,
            OriginRelation::Behind => PendingPushRelation::Behind,
            OriginRelation::Diverged => PendingPushRelation::Diverged,
        }),
        Err(error) => {
            report.problems.push(format!(
                "cannot tell how {branch} relates to origin: {error}"
            ));
            Some(PendingPushRelation::Unresolved)
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
    let context = RowContext {
        name: row_input.name,
        store: row_input.store,
        index: row_input.index,
        snapshot: row_input.snapshot,
        notches: row_input.notches,
        expected_base: row_input.expected_base,
    };
    let mut output = RowOutput { report, findings };
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
        let row = build_branch_row(
            &context,
            &mut output,
            &branch,
            RowFacts {
                divergent: false,
                tip: Some(tip.short().to_owned()),
                landed,
                push: None,
                origin_tip: raw_origin.as_ref(),
                origin_relation: Some(relation),
            },
        );
        output.report.branches.push(row);
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
        assert_eq!(branch_state(case), BranchState::Draft);

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
    fn missing_merge_facts_make_an_open_pull_unknown_and_report_problems() {
        let pull = PullRequest {
            number: 7,
            ..PullRequest::default()
        };
        let mut problems = Vec::new();
        let mut findings = Vec::new();
        add_pull_findings(
            &mut problems,
            &mut findings,
            PullFindingInput {
                branch: &BranchName::new("feat/alpha"),
                pull: Some(&pull),
                checks: None,
                review_predates_head: None,
                expected_base: "main",
            },
        );

        assert_eq!(
            state_for(state_input(Some(&pull), None)),
            BranchState::Unknown
        );
        assert_eq!(problems.len(), 3, "problems: {problems:?}");
        assert!(
            findings
                .iter()
                .all(|finding| finding.kind != FindingKind::WrongBase),
            "unknown base must not be silently treated as the expected base: {findings:?}"
        );
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
            activity_at: None,
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

        assert_eq!(relation, Some(PendingPushRelation::Unresolved));
        assert!(
            report.problems.iter().any(|problem| {
                problem.contains("cannot tell how feat/alpha relates to origin")
            })
        );
    }
}
