use std::collections::{BTreeMap, BTreeSet};

use crate::commands::status::{BranchRow, Options, Report, Timings};
use crate::config::Registry;
use crate::detect::{Finding, FindingKind, Subject};
use crate::forge::Forge;
use crate::ids::{BranchName, BranchTarget, RepoName, Requirement};
use crate::store::Store;

/// Declared dependencies that are not satisfied yet.
///
/// A branch can require a pull request in a sibling fork. Dropping the required one
/// from a release without dropping the branch that needs it ships a release that
/// cannot work, which is exactly what happened when one repo's #4545 was dropped
/// while a sibling's #49 still needed it. Satisfied means merged: an open pull
/// request may still change or be rejected.
struct DependencyContext<'a, 'snapshot> {
    store: &'a Store,
    registry: &'a Registry,
    forge: Option<&'a dyn Forge>,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
}

struct DependencyResults<'a> {
    findings: &'a mut Vec<Finding>,
    problems: &'a mut Vec<String>,
}

impl DependencyResults<'_> {
    fn record(&mut self, branch: &BranchName, requirement: &Requirement, state: Option<&str>) {
        match state {
            Some(state) if state.eq_ignore_ascii_case("MERGED") => {}
            Some(state) => self.findings.push(Finding::new(
                FindingKind::UnmetDependency,
                Subject::Branch(branch.clone()),
                format!(
                    "{branch} requires {requirement}, which is {}",
                    state.to_lowercase()
                ),
            )),
            None => self.problems.push(format!(
                "{branch} requires {requirement}, which the forge did not report on"
            )),
        }
    }
}

fn unmet_dependencies(
    repo: &RepoName,
    branches: &[BranchRow],
    context: &DependencyContext<'_, '_>,
) -> (Vec<Finding>, Vec<String>) {
    let DependencyContext {
        store,
        registry,
        forge,
        snapshot,
    } = *context;
    let mut grouped: BTreeMap<RepoName, Vec<(BranchName, Requirement)>> = BTreeMap::new();
    for row in branches {
        let target = BranchTarget::new(repo.clone(), row.name.clone());
        for requirement in store.dependencies(&target) {
            grouped
                .entry(requirement.repo.clone())
                .or_default()
                .push((row.name.clone(), requirement));
        }
    }

    let mut findings = Vec::new();
    let mut problems = Vec::new();
    {
        let mut outcomes = DependencyResults {
            findings: &mut findings,
            problems: &mut problems,
        };
        for (required_repo, requirements) in grouped {
            let Some(entry) = registry.get(&required_repo) else {
                for (branch, requirement) in requirements {
                    outcomes.problems.push(format!(
                        "{branch} requires {requirement}, whose repo is not in the registry"
                    ));
                }
                continue;
            };

            if required_repo == *repo {
                let Some(snapshot) = snapshot else {
                    for (branch, requirement) in requirements {
                        outcomes.problems.push(format!(
                            "cannot check whether {branch} still needs {requirement}: no forge consulted"
                        ));
                    }
                    continue;
                };
                for (branch, requirement) in requirements {
                    outcomes.record(
                        &branch,
                        &requirement,
                        snapshot
                            .fact(requirement.number)
                            .map(|fact| fact.pull.state.as_str()),
                    );
                }
                continue;
            }

            let Some(forge) = forge else {
                for (branch, requirement) in requirements {
                    outcomes.problems.push(format!(
                        "cannot check whether {branch} still needs {requirement}: no forge consulted"
                    ));
                }
                continue;
            };
            let numbers: Vec<u64> = requirements
                .iter()
                .map(|(_, requirement)| requirement.number)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            match forge
                .repo_identity(&entry.path)
                .and_then(|identity| forge.pull_facts(&entry.path, &identity, &numbers))
            {
                Ok(facts) => {
                    for (branch, requirement) in requirements {
                        outcomes.record(
                            &branch,
                            &requirement,
                            facts
                                .get(&requirement.number)
                                .map(|fact| fact.pull.state.as_str()),
                        );
                    }
                }
                Err(error) => {
                    for (branch, requirement) in requirements {
                        outcomes.problems.push(format!(
                            "cannot check whether {branch} still needs {requirement}: {error}"
                        ));
                    }
                }
            }
        }
    }
    (findings, problems)
}

/// Fold declared dependencies into a report.
///
/// Separate from `gather` only to keep that function readable.
pub(super) struct DependencyInput<'a, 'forge, 'snapshot> {
    pub(super) report: &'a mut Report,
    pub(super) findings: &'a mut Vec<Finding>,
    pub(super) name: &'a RepoName,
    pub(super) store: &'a Store,
    pub(super) options: &'a Options<'forge>,
    pub(super) snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    pub(super) timings: &'a mut Timings,
}

pub(super) fn add_dependency_findings(input: DependencyInput<'_, '_, '_>) {
    let DependencyInput {
        report,
        findings,
        name,
        store,
        options,
        snapshot,
        timings,
    } = input;
    let Some(registry) = options.registry else {
        return;
    };
    let started = std::time::Instant::now();
    let (found, unanswered) = unmet_dependencies(
        name,
        &report.branches,
        &DependencyContext {
            store,
            registry,
            forge: options.forge,
            snapshot,
        },
    );
    timings.forge += started.elapsed();
    findings.extend(found);
    report.problems.extend(unanswered);
}
