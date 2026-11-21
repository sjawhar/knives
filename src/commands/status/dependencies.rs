use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::path::Path;

use crate::bind::Fork;
use crate::commands::status::{BranchRow, BranchState, PostPhaseInput, Report, Timings};
use crate::config::{Registry, RepoEntry};
use crate::detect::{BookmarkTips, Finding, FindingKind, LandedVerdict, Subject, classify_landed};
use crate::forge::{Forge, PullFacts, PullIndex, RepoIdentity};
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, RemoteName, Requirement, UpstreamName,
};
use crate::jj::{JjError, Repo};
use crate::remote_url::remote_slug;
use crate::store::Store;

use super::merged::merged_landing;
use super::rows::pull_summary_for;

/// The remote, and the role, of a fork's own repository.
const ORIGIN: &str = "origin";

/// The forge repository a sibling entry's pull requests live in, from its
/// `upstream` URL; `None` when that is not a forge remote (a local path).
///
/// The sibling's checkout is not consulted: its identity is its `upstream`, and
/// it need not be on this machine for its pull requests to be asked about.
fn upstream_identity(entry: &RepoEntry) -> Option<RepoIdentity> {
    remote_slug(&entry.upstream).map(|slug| RepoIdentity {
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
/// Its head is that member only when it is a branch of the fork's own
/// repository's owner, and a member branch by its name; a review pull request
/// on another fork's repository, or with any other head, cannot be judged
/// here, and is a problem that says so.
struct DependencyContext<'a, 'snapshot> {
    store: &'a Store,
    registry: &'a Registry,
    fork: &'a Fork<'a>,
    forge: Option<&'a dyn Forge>,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
}

impl DependencyContext<'_, '_> {
    /// This fork's checkout, where the forge client runs.
    fn path(&self) -> &Path {
        &self.fork.checkout.path
    }
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
            MemberState::Unprobed => self.unmet(
                branch,
                format!(
                    "{branch} requires {shown}, which is not known to have landed: the landed \
                     probe did not run"
                ),
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
#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberState {
    /// It has landed upstream.
    Landed,
    /// It has not, with what was seen: its row's state, or the probe's verdict.
    Open(String),
    /// It has no landed or merged row, and the landed probe, which alone can
    /// say it landed, did not run (`status --no-landed`, or no upstream
    /// trunk). Kept as unmet, as the drop guard needs: a member not shown to
    /// have landed is still needed.
    Unprobed,
    /// It cannot be judged, and why.
    Unknown(String),
}

/// The member branches of a fork as this checkout sees them: the rows the
/// report already judged, then the origin's copy of a member this checkout
/// has no bookmark of, as a machine that only fetched another machine's
/// member has. Each member is judged once per report.
struct Members<'a> {
    rows: &'a [BranchRow],
    repo: &'a Repo,
    path: &'a Path,
    tips: &'a BookmarkTips,
    /// The upstream pull requests of this fork's branches, by head branch.
    index: &'a PullIndex,
    /// The upstream trunk's commit, when it resolves.
    trunk_commit: Option<&'a CommitId>,
    /// What the landed probe replays onto, the upstream trunk; `None` when
    /// the probe did not run.
    probed_onto: Option<&'a str>,
    judged: RefCell<BTreeMap<BranchName, MemberState>>,
}

impl Members<'_> {
    /// Landed when its row is landed or merged, or its origin tip is in the
    /// upstream trunk: by a merged upstream pull request whose landing the
    /// trunk holds, or by an empty replay.
    fn judge(&self, member: &BranchName) -> MemberState {
        if let Some(state) = self.judged.borrow().get(member) {
            return state.clone();
        }
        let state = self.judged_now(member);
        let _ = self
            .judged
            .borrow_mut()
            .insert(member.clone(), state.clone());
        state
    }

    fn judged_now(&self, member: &BranchName) -> MemberState {
        match self.rows.iter().find(|row| row.name == *member) {
            Some(row)
                if matches!(row.state, BranchState::Landed | BranchState::Merged)
                    || row.landed == Some(LandedVerdict::InTrunk) =>
            {
                MemberState::Landed
            }
            Some(_) if self.probed_onto.is_none() => MemberState::Unprobed,
            Some(row) => MemberState::Open(row.state.to_string()),
            None => self.judge_on_origin(member),
        }
    }

    fn judge_on_origin(&self, member: &BranchName) -> MemberState {
        let on_origin = BookmarkRef::Remote {
            branch: member.clone(),
            remote: RemoteName::new(ORIGIN),
        };
        let Some(tip) = self.tips.get(&on_origin) else {
            return MemberState::Unknown(format!(
                "this checkout has no branch {member}, here or on origin; fetch it, or forget \
                 the requirement if the branch landed or was dropped"
            ));
        };
        match self.merged_upstream(member, tip) {
            Ok(true) => return MemberState::Landed,
            Ok(false) => {}
            Err(error) => {
                return MemberState::Unknown(format!(
                    "whether {on_origin}'s merged pull request landed could not be read: {error}"
                ));
            }
        }
        let Some(trunk) = self.probed_onto else {
            return MemberState::Unprobed;
        };
        match crate::jj::probe_landed(self.path, &BranchName::new(tip.as_str()), trunk) {
            Ok(outcome) => match classify_landed(outcome) {
                LandedVerdict::InTrunk => MemberState::Landed,
                verdict => MemberState::Open(format!("{on_origin} {verdict}")),
            },
            Err(error) => {
                MemberState::Unknown(format!("the landed probe of {on_origin} failed: {error}"))
            }
        }
    }

    /// Whether `member`'s upstream pull request merged, the trunk holds its
    /// landing, and the origin tip holds nothing past what it merged: the
    /// evidence a squash merge leaves, which a replay cannot see
    /// ([`super::merged`]).
    fn merged_upstream(&self, member: &BranchName, tip: &CommitId) -> Result<bool, JjError> {
        let Some(trunk) = self.trunk_commit else {
            return Ok(false);
        };
        let Some(summary) = pull_summary_for(member, &self.index.by_branch) else {
            return Ok(false);
        };
        let Some(landing) = merged_landing(summary) else {
            return Ok(false);
        };
        let (Ok(landing), Ok(merged_head)) = (
            self.repo.resolve_commit(landing),
            self.repo.resolve_commit(&summary.head_ref_oid),
        ) else {
            return Ok(false);
        };
        Ok(self.repo.is_ancestor(&landing, trunk)?
            && (*tip == merged_head || self.repo.is_ancestor(tip, &merged_head)?))
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
    /// Member branches: the dependent, the requirement, and the branch it
    /// names; one per dependent and branch, whatever repository spelling
    /// each record carries.
    members: Vec<(BranchName, Requirement, BranchName)>,
}

fn unmet_dependencies(
    members: &Members<'_>,
    context: &DependencyContext<'_, '_>,
) -> (Vec<Finding>, Vec<String>) {
    let mut findings = Vec::new();
    let mut problems = Vec::new();
    let repo = &context.fork.upstream;
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
    let mut member_pairs = BTreeSet::new();
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
                    if member_pairs.insert((dependent.clone(), branch.clone())) {
                        sorted.members.push((dependent, requirement, branch));
                    }
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
            context.path(),
            Batch {
                identity,
                requirements,
                heads: None,
            },
            outcomes,
        );
    }
}

/// This fork's own repository as the remotes here spell it: the checkout's
/// `origin` remote and the registry's `origin`, each as `<owner>/<name>`.
fn own_spellings(fork: &Fork<'_>) -> Vec<String> {
    fork.checkout
        .remotes
        .get(ORIGIN)
        .map(String::as_str)
        .into_iter()
        .chain(std::iter::once(fork.entry.origin.as_str()))
        .filter_map(remote_slug)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Review pull requests on a fork's own repository, one batch per
/// repository, each judged by its head. A batch on any other repository is
/// a problem per requirement: its heads are no members of this fork.
fn fork_pull_outcomes(
    on_fork: BTreeMap<String, Vec<Wanted>>,
    members: &Members<'_>,
    context: &DependencyContext<'_, '_>,
    outcomes: &mut DependencyResults<'_>,
) {
    if on_fork.is_empty() {
        return;
    }
    let Some(forge) = context.forge else {
        for (branch, requirement, _) in on_fork.into_values().flatten() {
            outcomes.unchecked(&branch, &requirement, "no forge consulted");
        }
        return;
    };
    let spellings = own_spellings(context.fork);
    // The name the forge gives this fork's own repository, asked once and
    // only when a record names a repository no spelling here gives: the
    // repository was renamed, or the record is another fork's.
    let mut renamed: Option<Result<String, String>> = None;
    for (repository, requirements) in on_fork {
        let own = spellings
            .iter()
            .any(|spelled| spelled.eq_ignore_ascii_case(&repository))
            || match renamed.get_or_insert_with(|| {
                let origin = context
                    .fork
                    .checkout
                    .remotes
                    .get(ORIGIN)
                    .map_or(context.fork.entry.origin.as_str(), String::as_str);
                forge
                    .repo_identity(context.path(), origin, origin)
                    .map(|identity| identity.name_with_owner.to_ascii_lowercase())
                    .map_err(|error| error.to_string())
            }) {
                Ok(name) => name.eq_ignore_ascii_case(&repository),
                Err(error) => {
                    for (branch, requirement, _) in requirements {
                        outcomes.unchecked(
                            &branch,
                            &requirement,
                            format_args!(
                                "whether {repository} is {}'s own repository could not be asked: \
                                 {error}",
                                context.fork.name
                            ),
                        );
                    }
                    continue;
                }
            };
        if !own {
            for (branch, requirement, _) in requirements {
                outcomes.unchecked(
                    &branch,
                    &requirement,
                    format_args!(
                        "{repository} is not {}'s own repository, and a review pull request on \
                         another fork's repository cannot be judged here; forget it and name the \
                         upstream pull request its member becomes",
                        context.fork.name
                    ),
                );
            }
            continue;
        }
        let owner = repository
            .split_once('/')
            .map_or(repository.as_str(), |(owner, _)| owner)
            .to_owned();
        let identity = RepoIdentity {
            name_with_owner: repository,
            id: String::new(),
        };
        asked(
            forge,
            context.path(),
            Batch {
                identity,
                requirements,
                heads: Some(Heads {
                    members,
                    owner,
                    entry: context.fork.entry,
                }),
            },
            outcomes,
        );
    }
}

/// What a review pull request on the fork's own repository is judged by: its
/// head, when that head is a member branch of the fork.
struct Heads<'m, 'r> {
    members: &'m Members<'r>,
    /// The own repository's owner: a head of any other owner is another
    /// fork's branch, whatever its name.
    owner: String,
    entry: &'m RepoEntry,
}

impl Heads<'_, '_> {
    fn judge(&self, fact: &PullFacts) -> MemberState {
        let head = BranchName::new(fact.pull.head_ref_name.as_str());
        if !fact.pull.is_from(&self.owner) {
            let whose = fact.pull.head_repository_owner.as_ref().map_or_else(
                || "an unknown owner".to_owned(),
                |account| account.login.clone(),
            );
            return MemberState::Unknown(format!(
                "its head {head} is a branch of {whose}, not of {}, so no member of this fork",
                self.owner
            ));
        }
        if let Some(kind) = self.entry.non_member(&head) {
            return MemberState::Unknown(format!(
                "its head {head} is {kind} of this fork, not a member branch"
            ));
        }
        self.members.judge(&head)
    }
}

/// Pull requests to ask one repository about in one batch.
struct Batch<'m, 'r> {
    identity: RepoIdentity,
    requirements: Vec<Wanted>,
    /// For review pull requests on this fork's own repository: how their
    /// heads are judged. `None` for pull requests on an upstream, judged by
    /// their own state.
    heads: Option<Heads<'m, 'r>>,
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
                match (facts.get(&number), heads.as_ref()) {
                    (Some(fact), Some(heads)) => {
                        let shown = format!(
                            "{requirement} (head {}, {})",
                            fact.pull.head_ref_name,
                            fact.pull.state.to_lowercase()
                        );
                        outcomes.member(&branch, &shown, heads.judge(fact));
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
pub(super) struct DependencyInput<'a, 'post, 'snapshot> {
    pub(super) report: &'a mut Report,
    pub(super) findings: &'a mut Vec<Finding>,
    /// The fork, its repository, its tips, the state file and the options,
    /// and whether the landed probe ran.
    pub(super) post: &'a PostPhaseInput<'post>,
    pub(super) index: &'a PullIndex,
    pub(super) snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    pub(super) timings: &'a mut Timings,
}

pub(super) fn add_dependency_findings(input: DependencyInput<'_, '_, '_>) {
    let DependencyInput {
        report,
        findings,
        post,
        index,
        snapshot,
        timings,
    } = input;
    let Some(registry) = post.options.registry else {
        return;
    };
    let started = std::time::Instant::now();
    // What the landed probe replayed onto, the upstream trunk: a member with
    // no row is probed onto it too.
    let trunk_ref = post.fork.entry.trunk_ref();
    let members = Members {
        rows: &report.branches,
        repo: post.repo,
        path: &post.fork.checkout.path,
        tips: post.tips,
        index,
        trunk_commit: post.trunk_commit,
        probed_onto: post.probe_ran.then_some(trunk_ref.as_str()),
        judged: RefCell::new(BTreeMap::new()),
    };
    let (found, unanswered) = unmet_dependencies(
        &members,
        &DependencyContext {
            store: post.store,
            registry,
            fork: post.fork,
            forge: post.options.forge,
            snapshot,
        },
    );
    timings.forge += started.elapsed();
    findings.extend(found);
    report.problems.extend(unanswered);
}
