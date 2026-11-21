use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::path::Path;

use crate::commands::status::{BranchRow, BranchState, Options, Report, Timings};
use crate::config::{Registry, RepoEntry};
use crate::detect::{Finding, FindingKind, LandedVerdict, Subject, classify_landed};
use crate::forge::{Forge, RepoIdentity};
use crate::ids::{BranchName, BranchTarget, Requirement, UpstreamName};
use crate::jj::Repo;
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
/// while a sibling's #49 still needed it. An upstream pull request is satisfied
/// when it has merged: an open one may still change or be rejected. A member
/// branch of the fork is satisfied once it has landed upstream ([`Members`]).
/// A review pull request on the fork's own repository is satisfied once its
/// head, the member it carries, has landed upstream: a review pull request
/// merges into a review base or another member, so its own state says nothing
/// about the member reaching the upstream, and while the member is only in
/// the fork's release, dropping it from the release would break the dependent.
struct DependencyContext<'a, 'snapshot> {
    store: &'a Store,
    registry: &'a Registry,
    /// This fork's checkout, where the forge client runs.
    path: &'a Path,
    forge: Option<&'a dyn Forge>,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
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

    /// A member `branch` requires, `shown` as the report names it, by what
    /// this checkout says of the member.
    fn member(&mut self, branch: &BranchName, shown: &str, state: MemberState) {
        match state {
            MemberState::Landed => {}
            MemberState::Open(seen) => self.unmet(
                branch,
                format!("{branch} requires {shown}, which has not landed ({seen})"),
            ),
            MemberState::Unknown(why) => self.unchecked(branch, shown, why),
        }
    }

    fn unmet(&mut self, branch: &BranchName, detail: String) {
        self.findings.push(Finding::new(
            FindingKind::UnmetDependency,
            Subject::Branch(branch.clone()),
            detail,
        ));
    }

    fn unchecked(&mut self, branch: &BranchName, requirement: impl Display, why: impl Display) {
        self.problems.push(format!(
            "cannot check whether {branch} still needs {requirement}: {why}"
        ));
    }
}

/// What this checkout says of a member branch of the fork.
#[derive(Debug, PartialEq, Eq)]
enum MemberState {
    /// It has landed upstream.
    Landed,
    /// It has not, with what was seen: its row's state, or the probe's verdict.
    Open(String),
    /// It cannot be judged, and why.
    Unknown(String),
}

/// The member branches of a fork as this checkout sees them: the rows the
/// report already judged, then the origin's copy of a member this checkout
/// has no bookmark of, as a machine that only fetched another machine's
/// member has.
struct Members<'a> {
    rows: &'a [BranchRow],
    path: &'a Path,
    /// What the landed probe replays onto, the upstream trunk; `None` when
    /// the probe did not run.
    probed_onto: Option<&'a str>,
    repo: OnceCell<Option<Repo>>,
}

impl Members<'_> {
    /// Landed when its row is landed or merged, or its origin tip replays
    /// empty onto the upstream trunk.
    fn judge(&self, member: &BranchName) -> MemberState {
        match self.rows.iter().find(|row| row.name == *member) {
            Some(row) if matches!(row.state, BranchState::Landed | BranchState::Merged) => {
                MemberState::Landed
            }
            Some(_) if self.probed_onto.is_none() => {
                MemberState::Unknown("the landed probe did not run".to_owned())
            }
            Some(row) => MemberState::Open(row.state.to_string()),
            None => self.judge_on_origin(member),
        }
    }

    fn judge_on_origin(&self, member: &BranchName) -> MemberState {
        let on_origin = BranchName::new(format!("{member}@origin"));
        let known = self
            .repo
            .get_or_init(|| Repo::open(self.path).ok())
            .as_ref()
            .is_some_and(|repo| repo.resolve_commit(on_origin.as_str()).is_ok());
        if !known {
            return MemberState::Unknown(format!(
                "this checkout has no branch {member}, here or on origin; fetch it, or forget \
                 the requirement if the branch landed or was dropped"
            ));
        }
        let Some(trunk) = self.probed_onto else {
            return MemberState::Unknown("the landed probe did not run".to_owned());
        };
        match crate::jj::probe_landed(self.path, &on_origin, trunk) {
            Ok(outcome) => match classify_landed(outcome) {
                LandedVerdict::InTrunk => MemberState::Landed,
                verdict => MemberState::Open(format!("{on_origin} {verdict}")),
            },
            Err(error) => {
                MemberState::Unknown(format!("the landed probe of {on_origin} failed: {error}"))
            }
        }
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
    members: &Members<'_>,
    context: &DependencyContext<'_, '_>,
) -> (Vec<Finding>, Vec<String>) {
    let mut findings = Vec::new();
    let mut problems = Vec::new();
    let sorted = sort_requirements(repo, members.rows, context, &mut problems);
    let mut outcomes = DependencyResults {
        findings: &mut findings,
        problems: &mut problems,
    };
    upstream_outcomes(repo, sorted.upstream, context, &mut outcomes);
    fork_pull_outcomes(sorted.on_fork, members, context, &mut outcomes);
    for (dependent, requirement, member) in sorted.members {
        outcomes.member(&dependent, &requirement.to_string(), members.judge(&member));
    }
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
                heads: None,
            },
            outcomes,
        );
    }
}

/// Review pull requests on a fork's own repository, one batch per
/// repository, each judged by its head.
fn fork_pull_outcomes(
    on_fork: BTreeMap<String, Vec<Wanted>>,
    members: &Members<'_>,
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
                heads: Some(members),
            },
            outcomes,
        );
    }
}

/// Pull requests to ask one repository about in one batch.
struct Batch<'m, 'r> {
    identity: RepoIdentity,
    requirements: Vec<Wanted>,
    /// For review pull requests on the fork: the members their heads are
    /// judged among. `None` for pull requests on an upstream, judged by
    /// their own state.
    heads: Option<&'m Members<'r>>,
}

/// Ask the forge about every number `batch` holds, in one call, and record
/// each requirement.
fn asked(
    forge: &dyn Forge,
    path: &Path,
    batch: Batch<'_, '_>,
    outcomes: &mut DependencyResults<'_>,
) {
    let Batch {
        identity,
        requirements,
        heads,
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
                match (facts.get(&number), heads) {
                    (Some(fact), Some(members)) => {
                        let head = BranchName::new(fact.pull.head_ref_name.as_str());
                        let shown = format!(
                            "{requirement} (head {head}, {})",
                            fact.pull.state.to_lowercase()
                        );
                        outcomes.member(&branch, &shown, members.judge(&head));
                    }
                    (fact, _) => outcomes.record(
                        &branch,
                        &requirement.to_string(),
                        fact.map(|fact| fact.pull.state.as_str()),
                    ),
                }
            }
        }
        Err(error) => {
            for (branch, requirement, _) in requirements {
                outcomes.unchecked(&branch, &requirement, &error);
            }
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
    /// The upstream trunk the landed probe replayed onto, `None` when it did
    /// not run: a member with no row is probed onto it too.
    pub(super) probed_onto: Option<&'a str>,
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
        probed_onto,
        timings,
    } = input;
    let Some(registry) = options.registry else {
        return;
    };
    let started = std::time::Instant::now();
    let members = Members {
        rows: &report.branches,
        path,
        probed_onto,
        repo: OnceCell::new(),
    };
    let (found, unanswered) = unmet_dependencies(
        name,
        &members,
        &DependencyContext {
            store,
            registry,
            path,
            forge: options.forge,
            snapshot,
        },
    );
    timings.forge += started.elapsed();
    findings.extend(found);
    report.problems.extend(unanswered);
}
