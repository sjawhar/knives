use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::path::Path;

use crate::commands::status::{BranchRow, BranchState, Options, Report, Timings};
use crate::config::{Registry, RepoEntry};
use crate::detect::{Finding, FindingKind, Subject};
use crate::forge::{Forge, RepoIdentity};
use crate::ids::{BranchName, BranchTarget, Requirement, UpstreamName};
use crate::store::Store;

/// The forge repository a sibling entry's pull requests live in, from its
/// `upstream` URL; `None` when that is not a forge remote (a local path).
///
/// The sibling's checkout is not consulted: its identity is its `upstream`, and
/// it need not be on this machine for its pull requests to be asked about.
fn upstream_identity(entry: &RepoEntry) -> Option<RepoIdentity> {
    crate::remote_url::remote_slug(&entry.upstream).map(|slug| RepoIdentity {
        name_with_owner: slug.to_owned(),
        id: String::new(),
    })
}

/// Declared dependencies that are not satisfied yet.
///
/// A branch can require a pull request in a sibling fork. Dropping the required one
/// from a release without dropping the branch that needs it ships a release that
/// cannot work, which is exactly what happened when one repo's #4545 was dropped
/// while a sibling's #49 still needed it. Satisfied means merged: an open pull
/// request may still change or be rejected. A review pull request on a fork's
/// own repository is asked of that repository, and the report names its head:
/// the member branch the dependent waits for. A member branch is satisfied once
/// it has landed upstream, as the landed probe or its merged pull request says.
struct DependencyContext<'a, 'snapshot> {
    store: &'a Store,
    registry: &'a Registry,
    /// This fork's checkout, where the forge client runs.
    path: &'a Path,
    forge: Option<&'a dyn Forge>,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    /// Whether the landed probe judged this fork's branches.
    probe_ran: bool,
}

struct DependencyResults<'a> {
    findings: &'a mut Vec<Finding>,
    problems: &'a mut Vec<String>,
}

impl DependencyResults<'_> {
    /// A pull request `branch` requires, `shown` as the report names it, by
    /// the `state` the forge gave it.
    fn record(&mut self, branch: &BranchName, shown: &str, state: Option<&str>) {
        match state {
            Some(state) if state.eq_ignore_ascii_case("MERGED") => {}
            Some(state) => self.unmet(
                branch,
                format!(
                    "{branch} requires {shown}, which is {}",
                    state.to_lowercase()
                ),
            ),
            None => self.problems.push(format!(
                "{branch} requires {shown}, which the forge did not report on"
            )),
        }
    }

    fn unmet(&mut self, branch: &BranchName, detail: String) {
        self.findings.push(Finding::new(
            FindingKind::UnmetDependency,
            Subject::Branch(branch.clone()),
            detail,
        ));
    }

    fn unchecked(&mut self, branch: &BranchName, requirement: &Requirement, why: impl Display) {
        self.problems.push(format!(
            "cannot check whether {branch} still needs {requirement}: {why}"
        ));
    }
}

/// One pull request a branch requires: the branch, the requirement, and the
/// requirement's number.
type Wanted = (BranchName, Requirement, u64);

/// The required forks of `repo`'s branches, by the name each is kept under,
/// with every requirement on its upstream. A requirement names its fork by
/// that name and no other ([`crate::config::Registry::kept_under`]).
type Required<'a> = BTreeMap<UpstreamName, (&'a RepoEntry, Vec<Wanted>)>;

/// Every requirement of a fork's branches, by where it is answered.
#[derive(Default)]
struct Sorted<'a> {
    upstream: Required<'a>,
    /// Review pull requests, by the fork repository each is on.
    on_fork: BTreeMap<String, Vec<Wanted>>,
    /// Member branches: the dependent, the requirement, and the branch it names.
    members: Vec<(BranchName, Requirement, BranchName)>,
}

fn unmet_dependencies(
    repo: &UpstreamName,
    branches: &[BranchRow],
    context: &DependencyContext<'_, '_>,
) -> (Vec<Finding>, Vec<String>) {
    let mut findings = Vec::new();
    let mut problems = Vec::new();
    let sorted = sort_requirements(repo, branches, context, &mut problems);
    let mut outcomes = DependencyResults {
        findings: &mut findings,
        problems: &mut problems,
    };
    upstream_outcomes(repo, sorted.upstream, context, &mut outcomes);
    fork_pull_outcomes(sorted.on_fork, context, &mut outcomes);
    member_outcomes(sorted.members, branches, context.probe_ran, &mut outcomes);
    (findings, problems)
}

fn sort_requirements<'a>(
    repo: &UpstreamName,
    branches: &[BranchRow],
    context: &DependencyContext<'a, '_>,
    problems: &mut Vec<String>,
) -> Sorted<'a> {
    let mut sorted = Sorted::default();
    for row in branches {
        let target = BranchTarget::new(repo.clone(), row.name.clone());
        for requirement in context.store.dependencies(&target) {
            let dependent = row.name.clone();
            match &requirement {
                Requirement::Upstream {
                    repo: required,
                    number,
                } => {
                    let (required, number) = (required.clone(), *number);
                    match context.registry.kept_under(&required) {
                        Some(entry) => sorted
                            .upstream
                            .entry(required)
                            .or_insert_with(|| (entry, Vec::new()))
                            .1
                            .push((dependent, requirement, number)),
                        None => problems.push(format!(
                            "{dependent} requires {requirement}, but no fork in the registry is \
                             kept under {required}: a requirement names its fork by its \
                             upstream repository's <owner>/<name>"
                        )),
                    }
                }
                Requirement::ForkPull { repository, number } => {
                    let (repository, number) = (repository.clone(), *number);
                    sorted.on_fork.entry(repository).or_default().push((
                        dependent,
                        requirement,
                        number,
                    ));
                }
                Requirement::ForkBranch { branch, .. } => {
                    let branch = branch.clone();
                    sorted.members.push((dependent, requirement, branch));
                }
            }
        }
    }
    sorted
}

/// Pull requests on a fork's upstream: this fork's from the status snapshot,
/// a sibling's from one batch on the sibling's upstream.
fn upstream_outcomes(
    repo: &UpstreamName,
    grouped: Required<'_>,
    context: &DependencyContext<'_, '_>,
    outcomes: &mut DependencyResults<'_>,
) {
    for (required_repo, (entry, requirements)) in grouped {
        if required_repo == *repo {
            let Some(snapshot) = context.snapshot else {
                for (branch, requirement, _) in requirements {
                    outcomes.unchecked(&branch, &requirement, "no forge consulted");
                }
                continue;
            };
            for (branch, requirement, number) in requirements {
                outcomes.record(
                    &branch,
                    &requirement.to_string(),
                    snapshot.fact(number).map(|fact| fact.pull.state.as_str()),
                );
            }
            continue;
        }

        let Some(forge) = context.forge else {
            for (branch, requirement, _) in requirements {
                outcomes.unchecked(&branch, &requirement, "no forge consulted");
            }
            continue;
        };
        let Some(identity) = upstream_identity(entry) else {
            outcomes.problems.push(format!(
                "{required_repo}: upstream {} is not a forge repository; cannot check \
                 dependencies against it",
                entry.upstream
            ));
            continue;
        };
        asked(
            forge,
            context.path,
            Batch {
                identity,
                requirements,
                with_head: false,
            },
            outcomes,
        );
    }
}

/// Review pull requests on a fork's own repository, one batch per repository.
fn fork_pull_outcomes(
    on_fork: BTreeMap<String, Vec<Wanted>>,
    context: &DependencyContext<'_, '_>,
    outcomes: &mut DependencyResults<'_>,
) {
    for (repository, requirements) in on_fork {
        let Some(forge) = context.forge else {
            for (branch, requirement, _) in requirements {
                outcomes.unchecked(&branch, &requirement, "no forge consulted");
            }
            continue;
        };
        let identity = RepoIdentity {
            name_with_owner: repository,
            id: String::new(),
        };
        asked(
            forge,
            context.path,
            Batch {
                identity,
                requirements,
                with_head: true,
            },
            outcomes,
        );
    }
}

/// Pull requests to ask one repository about in one batch.
struct Batch {
    identity: RepoIdentity,
    requirements: Vec<Wanted>,
    /// Whether the report names each pull request's head branch: the member
    /// a review pull request on the fork carries.
    with_head: bool,
}

/// Ask the forge about every number `batch` holds, in one call, and record
/// each requirement.
fn asked(forge: &dyn Forge, path: &Path, batch: Batch, outcomes: &mut DependencyResults<'_>) {
    let Batch {
        identity,
        requirements,
        with_head,
    } = batch;
    let numbers: Vec<u64> = requirements
        .iter()
        .map(|(_, _, number)| *number)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    match forge.pull_facts(path, &identity, &numbers) {
        Ok(facts) => {
            for (branch, requirement, number) in requirements {
                let fact = facts.get(&number);
                let shown = match fact {
                    Some(fact) if with_head => {
                        format!("{requirement} (head {})", fact.pull.head_ref_name)
                    }
                    _ => requirement.to_string(),
                };
                outcomes.record(&branch, &shown, fact.map(|fact| fact.pull.state.as_str()));
            }
        }
        Err(error) => {
            for (branch, requirement, _) in requirements {
                outcomes.unchecked(&branch, &requirement, &error);
            }
        }
    }
}

/// Member branches of this fork, from the rows the report already judged.
fn member_outcomes(
    members: Vec<(BranchName, Requirement, BranchName)>,
    branches: &[BranchRow],
    probe_ran: bool,
    outcomes: &mut DependencyResults<'_>,
) {
    for (dependent, requirement, member) in members {
        match branches.iter().find(|row| row.name == member) {
            Some(row) if matches!(row.state, BranchState::Landed | BranchState::Merged) => {}
            Some(_) if !probe_ran => {
                outcomes.unchecked(&dependent, &requirement, "the landed probe did not run");
            }
            Some(row) => outcomes.unmet(
                &dependent,
                format!(
                    "{dependent} requires {requirement}, which has not landed ({})",
                    row.state
                ),
            ),
            None => outcomes.unchecked(
                &dependent,
                &requirement,
                format_args!("this checkout has no branch {member}"),
            ),
        }
    }
}

/// Fold declared dependencies into a report.
///
/// Separate from `gather` only to keep that function readable.
pub(super) struct DependencyInput<'a, 'forge, 'snapshot> {
    pub(super) report: &'a mut Report,
    pub(super) findings: &'a mut Vec<Finding>,
    pub(super) name: &'a UpstreamName,
    pub(super) path: &'a Path,
    pub(super) store: &'a Store,
    pub(super) options: &'a Options<'forge>,
    pub(super) snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    pub(super) probe_ran: bool,
    pub(super) timings: &'a mut Timings,
}

pub(super) fn add_dependency_findings(input: DependencyInput<'_, '_, '_>) {
    let DependencyInput {
        report,
        findings,
        name,
        path,
        store,
        options,
        snapshot,
        probe_ran,
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
            path,
            forge: options.forge,
            snapshot,
            probe_ran,
        },
    );
    timings.forge += started.elapsed();
    findings.extend(found);
    report.problems.extend(unanswered);
}
