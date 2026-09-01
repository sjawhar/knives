use std::collections::{BTreeMap, BTreeSet};

use crate::commands::status::{BranchRow, ClaimCell, LastNotch, RepoNotches, Report, SeenWindow};
use crate::detect::Finding;
use crate::ids::{BranchName, RepoName, WorkspaceName};
use crate::jj::{MAX_ACTIVITY_OPS, Repo, WorkspaceActivity};
use crate::ledger::{Entry as Notch, Ledger};
use crate::seen::{LastSeen, Seen, last_seen};
use crate::store::{Claim, Store};

/// Every notch in this repository's ledger, read once for the whole report.
///
/// One local file read per repository rather than one per branch. A ledger that
/// exists and cannot be read is an unanswered question rather than an absence:
/// a report that quietly showed no breadcrumbs would say this fork's history was
/// never written.
pub(super) fn notches_from_ledger(ledger: Option<&Ledger>, report: &mut Report) -> Vec<Notch> {
    let Some(ledger) = ledger else {
        return Vec::new();
    };
    match ledger.entries() {
        Ok(entries) => entries,
        Err(error) => {
            report.problems.push(format!("ledger unavailable: {error}"));
            Vec::new()
        }
    }
}

pub(super) fn repo_notches(notches: &[Notch]) -> Option<RepoNotches> {
    let last = LastNotch::of(notches.iter().filter(|notch| notch.subject.is_none()))?;
    Some(RepoNotches {
        count: last.count,
        last,
    })
}

/// Uses all observation sources when the operation walk succeeded. If it failed,
/// sidecar records remain trustworthy but the unobserved window is necessarily incomplete.
pub(super) fn claim_last_seen(
    claim: &Claim,
    activity: Option<&WorkspaceActivity>,
    seen: &Seen,
) -> LastSeen {
    let Some(activity) = activity else {
        let workspace = crate::commands::wip::workspace_for(&claim.branch);
        let workspace_key = format!("{}/{}", claim.repo, workspace);
        let timestamps = [
            seen.owners
                .get(&claim.kind)
                .and_then(|owners| owners.get(&claim.owner))
                .and_then(|timestamp| timestamp.parse().ok()),
            seen.workspaces
                .get(&workspace_key)
                .and_then(|timestamp| timestamp.parse().ok()),
        ];
        return timestamps
            .into_iter()
            .flatten()
            .max()
            .map_or(LastSeen::NoneWithinWindow, LastSeen::At);
    };
    last_seen(claim, activity, seen)
}

#[derive(Clone, Copy)]
pub(super) struct ClaimFoldInput<'a> {
    pub(super) repo: &'a Repo,
    pub(super) name: &'a RepoName,
    pub(super) store: &'a Store,
    pub(super) seen: &'a Seen,
}

/// Folds claims, workspace facts, and sidecar observations into branch rows.
pub(super) fn fold_claims(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    input: ClaimFoldInput<'_>,
) -> anyhow::Result<()> {
    let ClaimFoldInput {
        repo,
        name,
        store,
        seen,
    } = input;
    let claims: Vec<Claim> = store.claims(Some(name)).into_iter().cloned().collect();
    let wanted: BTreeSet<WorkspaceName> = claims
        .iter()
        .map(|claim| WorkspaceName::new(crate::commands::wip::workspace_for(&claim.branch)))
        .collect();
    let activity = match repo.workspace_activity(&wanted, MAX_ACTIVITY_OPS) {
        Ok(activity) => Some(activity),
        Err(error) => {
            report
                .problems
                .push(format!("workspace activity unavailable: {error}"));
            None
        }
    };
    let mut workspaces: BTreeSet<WorkspaceName> = match repo.workspaces() {
        Ok(rows) => rows.into_iter().map(|(workspace, _)| workspace).collect(),
        Err(error) => {
            report
                .problems
                .push(format!("workspaces unavailable: {error}"));
            BTreeSet::new()
        }
    };
    findings.extend(crate::commands::wip::overlaps(&touching(&claims)));

    for claim in &claims {
        if !report
            .branches
            .iter()
            .any(|row| row.name.as_str() == claim.branch)
        {
            report
                .branches
                .push(BranchRow::bare(BranchName::new(&claim.branch)));
        }
        let row = report
            .branches
            .iter_mut()
            .find(|row| row.name.as_str() == claim.branch)
            .ok_or_else(|| anyhow::anyhow!("claim row could not be materialized"))?;
        row.claim = Some(ClaimCell {
            id: claim.owner.clone(),
            kind: claim.kind,
            since: claim.started.clone(),
            why: claim.why.clone(),
        });
        match claim_last_seen(claim, activity.as_ref(), seen) {
            LastSeen::At(timestamp) => row.last_seen = Some(timestamp.to_string()),
            LastSeen::NoneSinceClaim => {
                row.seen = Some(SeenWindow::NoneSinceClaim);
            }
            LastSeen::NoneWithinWindow => {
                row.seen = Some(SeenWindow::NoneWithinWindow);
            }
        }
    }
    for row in &mut report.branches {
        let expected = WorkspaceName::new(crate::commands::wip::workspace_for(row.name.as_str()));
        if workspaces.remove(&expected) {
            row.workspace = Some(expected.to_string());
        }
    }
    report.other_workspaces = workspaces
        .into_iter()
        .map(|workspace| workspace.to_string())
        .collect();
    Ok(())
}

/// Files each claim says it is touching, keyed by claim.
fn touching(claims: &[Claim]) -> BTreeMap<String, Vec<String>> {
    claims
        .iter()
        .map(|claim| (claim.key(), claim.files.clone()))
        .collect()
}
