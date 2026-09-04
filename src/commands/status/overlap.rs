use std::collections::BTreeMap;

use crate::bind::Fork;
use crate::commands::status::Report;
use crate::detect::{BookmarkTips, Finding, LandedVerdict};
use crate::ids::{BookmarkRef, ReleaseScheme};
use crate::jj::Repo;

#[derive(Clone, Copy)]
pub(super) struct CarriedFindingInput<'a> {
    pub(super) report: &'a Report,
    pub(super) repo: &'a Repo,
    pub(super) tips: &'a BookmarkTips,
    pub(super) trunk: &'a str,
    pub(super) scheme: &'a ReleaseScheme,
    pub(super) publish_remote: &'a str,
}

/// Reports branches carried by another branch, excluding the configured trunk.
pub(super) fn carried_findings(input: CarriedFindingInput<'_>) -> anyhow::Result<Vec<Finding>> {
    let CarriedFindingInput {
        report,
        repo,
        tips,
        trunk,
        scheme,
        publish_remote,
    } = input;
    let mut findings = Vec::new();
    for row in &report.branches {
        let Some(tip) = tips.get(&BookmarkRef::Local(row.name.clone())) else {
            continue;
        };
        if row.landed == Some(LandedVerdict::InTrunk) {
            continue;
        }
        let carriers = repo
            .branches_containing(tip, scheme, publish_remote)?
            .into_iter()
            .filter(|reference| {
                reference.branch() != &row.name && reference.branch().as_str() != trunk
            })
            .collect::<Vec<_>>();
        if let Some(finding) = crate::detect::superseded::carried_elsewhere(&row.name, &carriers) {
            findings.push(finding);
        }
    }
    Ok(findings)
}

/// Changed-file result for one branch, if it has a single tip to compare.
type BranchFiles = Result<Vec<String>, String>;
/// The branch name and its optional changed-file result.
type BranchOverlapOutcome = (String, Option<BranchFiles>);

/// Compare branch paths concurrently, preserving the report's branch order.
///
/// `changed_files_between` invokes jj's porcelain because it normalizes paths.
/// It does not mutate the checkout, so each worker can independently query its
/// contiguous branch chunk and report the serial implementation's findings in
/// exactly the same order.
pub(super) fn add_branch_overlap_findings(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    fork: &Fork<'_>,
    workers: usize,
) -> std::time::Duration {
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let started = std::time::Instant::now();
    let rows = &report.branches;
    let upstream_trunk = entry.upstream_trunk();
    let outcomes: Vec<BranchOverlapOutcome> = if rows.is_empty() {
        Vec::new()
    } else {
        let workers = workers.clamp(1, rows.len());
        let chunk = rows.len().div_ceil(workers);
        let upstream_trunk = upstream_trunk.as_str();
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for slice in rows.chunks(chunk) {
                handles.push((
                    slice,
                    scope.spawn(move || {
                        slice
                            .iter()
                            .map(|row| {
                                let files = row.tip.as_ref().map(|_| {
                                    let from =
                                        format!("fork_point({upstream_trunk} | {})", row.name);
                                    crate::jj::changed_files_between(path, &from, row.name.as_str())
                                        .map_err(|error| error.to_string())
                                });
                                (row.name.to_string(), files)
                            })
                            .collect::<Vec<_>>()
                    }),
                ));
            }
            handles
                .into_iter()
                .flat_map(|(slice, handle)| {
                    handle.join().unwrap_or_else(|_| {
                        slice
                            .iter()
                            .map(|row| {
                                (
                                    row.name.to_string(),
                                    Some(Err("path comparison task panicked".to_owned())),
                                )
                            })
                            .collect()
                    })
                })
                .collect()
        })
    };
    let mut touching: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut notes = Vec::new();
    let mut unanswered = Vec::new();
    for (branch, files) in outcomes {
        match files {
            Some(Ok(files)) => {
                let _ = touching.insert(branch, files);
            }
            Some(Err(error)) => {
                unanswered.push(format!("cannot compare paths for {branch}: {error}"));
            }
            None => notes.push(format!(
                "cannot compare paths for {branch}: it has no single tip"
            )),
        }
    }
    report.notes.extend(notes);
    report.problems.extend(unanswered);
    findings.extend(crate::detect::overlap::branch_overlaps(&touching));
    started.elapsed()
}
