//! `knives wip`: what is being worked on right now.
//!
//! Deliberately separate from `knives repos`, which answers what we maintain.
//! Conflating the two was an earlier mistake in this design.

use std::collections::BTreeMap;

use crate::cli::Exit;
use crate::config::Registry;
use crate::detect::{Finding, FindingKind, Subject, double_checkout};
use crate::ids::{ChangeId, RepoName, WorkspaceName};
use crate::jj::{Repo, changed_files};
use crate::store::{Claim, Store};

/// Two active claims touching one file.
///
/// File overlap is the strongest duplicate-work signal available, and every
/// real collision observed was same-file. Two agents building the same fix on
/// differently named branches is otherwise undetectable.
pub fn overlaps(touching: &BTreeMap<String, Vec<String>>) -> Vec<Finding> {
    touching
        .iter()
        .filter(|(_, holders)| holders.len() > 1)
        .map(|(file, holders)| {
            Finding::new(
                FindingKind::ClaimOverlap,
                Subject::File(file.clone()),
                format!(
                    "{file} is being changed by {}: {}",
                    holders.len(),
                    holders.join(", ")
                ),
            )
        })
        .collect()
}

/// Which workspace a claim's work lives in.
///
/// `knives start` names a workspace for its branch with slashes flattened, so the
/// mapping is derivable rather than stored.
pub fn workspace_for(branch: &str) -> String {
    branch.replace('/', "-")
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub claims: Vec<Claim>,
    pub workspaces: BTreeMap<String, Vec<(WorkspaceName, ChangeId)>>,
    pub findings: Vec<Finding>,
    /// Informational: something worth saying that is not a failure.
    pub notes: Vec<String>,
    /// Could not answer. These, and only these, make the command exit non-zero
    /// for incompleteness. Keying on every note instead would make a routine
    /// remark like "14 superseded releases not scanned" look like a failure.
    pub problems: Vec<String>,
}

pub fn gather(registry: &Registry, store: &Store, only: Option<&RepoName>) -> Report {
    let mut report = Report {
        claims: store.claims(only).into_iter().cloned().collect(),
        ..Report::default()
    };

    let mut touching: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, entry) in &registry.repos {
        if only.is_some_and(|wanted| wanted.as_str() != name) {
            continue;
        }
        match Repo::open(&entry.path).and_then(|repo| repo.workspaces()) {
            Ok(rows) => {
                report.findings.extend(double_checkout(&rows));
                let _ = report.workspaces.insert(name.clone(), rows);
            }
            Err(error) => report.problems.push(format!("{name}: {error}")),
        }

        for claim in report.claims.iter().filter(|claim| &claim.repo == name) {
            let revision = format!("{}@", workspace_for(&claim.branch));
            // A claim with no workspace yet is normal, not an error worth
            // shouting about; it just contributes no overlap signal.
            if let Ok(files) = changed_files(&entry.path, &revision) {
                for file in files {
                    touching.entry(file).or_default().push(format!(
                        "{} on {}",
                        claim.owner,
                        claim.key()
                    ));
                }
            }
        }
    }

    report.findings.extend(overlaps(&touching));
    report
}

pub fn render(report: &Report) -> String {
    let mut lines: Vec<String> = report
        .problems
        .iter()
        .map(|problem| format!("!! {problem}"))
        .chain(report.notes.iter().map(|note| format!("! {note}")))
        .collect();
    if report.claims.is_empty() {
        lines.push("no active claims".to_owned());
    } else {
        lines.push(format!("{} active claim(s)", report.claims.len()));
        for claim in &report.claims {
            lines.push(format!(
                "  {}  {}  since {}",
                claim.key(),
                claim.owner,
                claim.started
            ));
            lines.push(format!("    {}", claim.why));
        }
    }
    for (repo, rows) in &report.workspaces {
        lines.push(format!("{repo}: {} workspace(s)", rows.len()));
        for (name, change) in rows {
            lines.push(format!(
                "  {name}  {}",
                change.as_str().chars().take(12).collect::<String>()
            ));
        }
    }
    if !report.findings.is_empty() {
        lines.push(format!("{} finding(s)", report.findings.len()));
        for finding in &report.findings {
            lines.push(format!("  [{}] {}", finding.kind, finding.subject.short()));
            lines.push(format!("    {}", finding.detail));
        }
    }
    lines.join("\n")
}

/// Findings mean act; notes mean we could not answer. A command that reports a
/// problem in its text and still exits zero lets a CI gate go green on a broken
/// forge login or an unopenable repository.
pub const fn exit_for(report: &Report) -> Exit {
    if !report.problems.is_empty() {
        return Exit::Incomplete;
    }
    if report.findings.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn touching(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(file, holders)| {
                (
                    (*file).to_owned(),
                    holders.iter().map(|h| (*h).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn two_claims_on_one_file_are_reported_with_both_holders() {
        let findings = overlaps(&touching(&[("src/x.rs", &["one on r/a", "two on r/b"])]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::ClaimOverlap);
        assert!(findings[0].detail.contains("one on r/a"));
        assert!(findings[0].detail.contains("two on r/b"));
    }

    #[test]
    fn a_file_only_one_claim_touches_is_not_a_finding() {
        assert!(overlaps(&touching(&[("src/x.rs", &["one on r/a"])])).is_empty());
    }

    #[test]
    fn a_workspace_name_is_derived_from_the_branch() {
        // `knives start` flattens slashes, so the mapping needs no storage.
        assert_eq!(workspace_for("feat/alpha"), "feat-alpha");
        assert_eq!(workspace_for("feat/a/b"), "feat-a-b");
    }
}
