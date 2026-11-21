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

    /// [`Self::unchecked`] for each of `requirements`, for one reason.
    fn unchecked_each(
        &mut self,
        requirements: impl IntoIterator<Item = Wanted>,
        why: impl Display,
    ) {
        for (branch, requirement, _) in requirements {
            self.unchecked(&branch, &requirement, &why);
        }
    }
}

/// What this checkout says of a member branch of the fork.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberState {
    /// It has landed upstream.
    Landed,
    /// It has not, with what was seen: its row's state, or the probe's verdict.
    Open(String),
    /// It has no landed row, and the landed probe, which alone can say it
    /// landed, did not run (`status --no-landed`, or no upstream trunk). Kept
    /// as unmet, as the drop guard needs: a member not shown to have landed
    /// is still needed.
    Unprobed,
    /// It cannot be judged, and why.
    Unknown(String),
}

/// The member branches of a fork as this checkout sees them: the rows the
/// report already judged, then the origin's copy of a member this checkout
/// has no bookmark of, as a machine that only fetched another machine's
/// member has. A review pull request's head is judged by the commit the
/// forge reports for it, whatever a branch of that name points at here.
/// Each member, and each head commit under each branch name, is judged once
/// per report.
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
    /// Head commits judged by [`Self::judge_commit`], by the branch name the
    /// review pull request gives them as well as the commit: the merge
    /// evidence it consults is the named branch's, so one commit can be
    /// landed under one name and not under another.
    judged_commits: RefCell<BTreeMap<(BranchName, CommitId), MemberState>>,
}

impl Members<'_> {
    /// Landed when its row is landed (by the probe, or by its merged upstream
    /// pull request's landing in the trunk), or its origin tip is in the
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

    /// A merged row is not met by its merge alone: a pull request merged
    /// into anything but the upstream trunk leaves the work outside it.
    /// [`super::merged`] sets the row's landed verdict when the trunk holds
    /// the merge, the same evidence [`Self::merged_upstream`] asks of a
    /// member known only on origin. The row judges the local tip only, so
    /// when origin holds work the local tip lacks (a local bookmark that did
    /// not follow a fetch), that origin tip is judged too, as a machine
    /// holding only the origin copy judges it: both must have landed.
    fn judged_now(&self, member: &BranchName) -> MemberState {
        let Some(row) = self.rows.iter().find(|row| row.name == *member) else {
            return self.judge_on_origin(member);
        };
        let local = self.row_state(row);
        if local != MemberState::Landed {
            return local;
        }
        let (Some(tip), Some(on_origin)) = (
            self.tips.get(&BookmarkRef::Local(member.clone())),
            self.tips.get(&remote(member)),
        ) else {
            return local;
        };
        if tip == on_origin {
            return local;
        }
        match self.repo.is_ancestor(on_origin, tip) {
            Ok(true) => local,
            Ok(false) => self.judge_commit(member, on_origin, &remote(member).to_string()),
            Err(error) => MemberState::Unknown(format!(
                "whether {} holds work {member} lacks could not be read: {error}",
                remote(member)
            )),
        }
    }

    /// What a row says of its local tip: landed when its state is landed or
    /// its landed verdict is in the trunk.
    fn row_state(&self, row: &BranchRow) -> MemberState {
        if row.state == BranchState::Landed || row.landed == Some(LandedVerdict::InTrunk) {
            MemberState::Landed
        } else if self.probed_onto.is_none() {
            MemberState::Unprobed
        } else {
            MemberState::Open(row.state.to_string())
        }
    }

    fn judge_on_origin(&self, member: &BranchName) -> MemberState {
        let on_origin = remote(member);
        let Some(tip) = self.tips.get(&on_origin) else {
            return MemberState::Unknown(format!(
                "this checkout has no branch {member}, here or on origin: fetch it. If it was \
                 deleted, its work may still be in a review base it merged into: forget the \
                 requirement only once that work has landed upstream"
            ));
        };
        self.judge_commit(member, tip, &on_origin.to_string())
    }

    /// The member a review pull request carries, judged by `head`, the
    /// commit the forge reports as its head, named `name`: by its row or
    /// origin copy when either is at that commit, else by the commit itself.
    /// A commit this checkout lacks cannot be judged: the branch moved
    /// since this checkout last fetched.
    fn judge_head(&self, name: &BranchName, head: &str) -> Result<MemberState, CommitId> {
        let Ok(commit) = self.repo.resolve_commit(head) else {
            return Err(CommitId::new(head));
        };
        let row = self.rows.iter().find(|row| row.name == *name);
        let local = self.tips.get(&BookmarkRef::Local(name.clone()));
        let on_origin = self.tips.get(&remote(name));
        if let Some(row) = row.filter(|_| local == Some(&commit)) {
            return Ok(self.row_state(row));
        }
        if row.is_none() && local.is_none() && on_origin == Some(&commit) {
            return Ok(self.judge(name));
        }
        let key = (name.clone(), commit);
        if let Some(state) = self.judged_commits.borrow().get(&key) {
            return Ok(state.clone());
        }
        let state = self.judge_commit(name, &key.1, &format!("{name} at {}", key.1.short()));
        let _ = self.judged_commits.borrow_mut().insert(key, state.clone());
        Ok(state)
    }

    /// `member`'s commit `tip`, `shown` as the report names it: landed when
    /// its merged upstream pull request's landing is in the trunk and `tip`
    /// holds nothing past what it merged, or when it replays empty onto the
    /// trunk.
    fn judge_commit(&self, member: &BranchName, tip: &CommitId, shown: &str) -> MemberState {
        match self.merged_upstream(member, tip) {
            Ok(true) => return MemberState::Landed,
            Ok(false) => {}
            Err(error) => {
                return MemberState::Unknown(format!(
                    "whether {shown}'s merged pull request landed could not be read: {error}"
                ));
            }
        }
        let Some(trunk) = self.probed_onto else {
            return MemberState::Unprobed;
        };
        match crate::jj::probe_landed(self.path, &BranchName::new(tip.as_str()), trunk) {
            Ok(outcome) => match classify_landed(outcome) {
                LandedVerdict::InTrunk => MemberState::Landed,
                verdict => MemberState::Open(format!("{shown} {verdict}")),
            },
            Err(error) => {
                MemberState::Unknown(format!("the landed probe of {shown} failed: {error}"))
            }
        }
    }

    /// Whether `member`'s upstream pull request merged, the trunk holds its
    /// landing, and `tip` holds nothing past what it merged: the evidence a
    /// squash merge leaves, which a replay cannot see ([`super::merged`]).
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

/// `name` on the remote that holds the fork's own repository.
fn remote(name: &BranchName) -> BookmarkRef {
    BookmarkRef::Remote {
        branch: name.clone(),
        remote: RemoteName::new(ORIGIN),
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
    let sorted = sort_requirements(repo, members, context, &mut problems);
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

/// Every branch whose requirements a report checks: each row; each branch
/// with a stated requirement that this checkout holds only as
/// `<branch>@origin`, another machine's branch that nobody here claims; and
/// each one held nowhere, deleted, whose recorded head (the commit it was at
/// when its list was last recorded) is here and has not landed upstream: a
/// branch merged into a review base before it was deleted still carries its
/// work there, and still needs what it required. A dependent held nowhere
/// whose recorded head landed is done; one whose recorded head is not here
/// leaves no copy that says where its work is, and is not checked.
fn dependents(repo: &UpstreamName, members: &Members<'_>, store: &Store) -> Vec<BranchName> {
    let mut dependents: Vec<BranchName> = members.rows.iter().map(|row| row.name.clone()).collect();
    for (branch, head) in store.dependents(repo) {
        if dependents.contains(&branch) {
            continue;
        }
        if members.tips.contains_key(&remote(&branch)) {
            dependents.push(branch);
            continue;
        }
        if members
            .tips
            .contains_key(&BookmarkRef::Local(branch.clone()))
        {
            continue;
        }
        let Some(commit) = head.and_then(|head| members.repo.resolve_commit(head.as_str()).ok())
        else {
            continue;
        };
        let shown = format!("{branch} at {}", commit.short());
        if members.judge_commit(&branch, &commit, &shown) != MemberState::Landed {
            dependents.push(branch);
        }
    }
    dependents
}

fn sort_requirements<'a>(
    repo: &UpstreamName,
    members: &Members<'_>,
    context: &DependencyContext<'a, '_>,
    problems: &mut Vec<String>,
) -> Sorted<'a> {
    let mut sorted = Sorted::default();
    let mut member_pairs = BTreeSet::new();
    for name in dependents(repo, members, context.store) {
        let target = BranchTarget::new(repo.clone(), name.clone());
        for requirement in context.store.dependencies(&target) {
            let dependent = name.clone();
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

/// This fork's `origin` URL: its checkout's `origin` remote, else the
/// registry's.
fn own_origin<'a>(fork: &'a Fork<'a>) -> &'a str {
    fork.checkout
        .remotes
        .get(ORIGIN)
        .map_or(fork.entry.origin.as_str(), String::as_str)
}

/// Review pull requests on a fork's own repository, one batch, each judged
/// by its head. The own repository is the one the forge names for this
/// fork's `origin` URL, asked once per report. A record names it when it
/// spells it as the forge names it now, or by a name the forge resolves to
/// that one: a record written before the repository was renamed still
/// names it. A spelling a remote here gives counts only so: a registry and
/// a checkout that name two live repositories do not merge them. Any other
/// repository is a problem per requirement: its heads are no members of
/// this fork.
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
        outcomes.unchecked_each(on_fork.into_values().flatten(), "no forge consulted");
        return;
    };
    let fork = &context.fork.name;
    let origin = own_origin(context.fork);
    let named = |url: &str| {
        forge
            .repo_identity(context.path(), url, url)
            .map(|identity| identity.name_with_owner.to_ascii_lowercase())
    };
    let own = match named(origin) {
        Ok(own) => own,
        Err(error) => {
            outcomes.unchecked_each(
                on_fork.into_values().flatten(),
                format_args!(
                    "the forge's name of {fork}'s own repository could not be asked: {error}"
                ),
            );
            return;
        }
    };
    let mut ours = Vec::new();
    for (repository, requirements) in on_fork {
        // Any other name: the forge says which repository it is.
        let resolved = if repository.eq_ignore_ascii_case(&own) {
            Ok(true)
        } else {
            remote_slug(origin)
                .map(|slug| origin.replacen(slug, &repository, 1))
                .map_or(Ok(false), |url: String| {
                    named(url.as_str()).map(|name| name == own)
                })
        };
        match resolved {
            Ok(true) => ours.extend(requirements),
            Ok(false) => outcomes.unchecked_each(
                requirements,
                format_args!(
                    "{repository} is not {fork}'s own repository, and a review pull request on \
                     another fork's repository cannot be judged here; forget it and name the \
                     upstream pull request its member becomes"
                ),
            ),
            Err(error) => outcomes.unchecked_each(
                requirements,
                format_args!(
                    "whether {repository} is {fork}'s own repository could not be asked: {error}"
                ),
            ),
        }
    }
    if ours.is_empty() {
        return;
    }
    let owner = own
        .split_once('/')
        .map_or(own.as_str(), |(owner, _)| owner)
        .to_owned();
    asked(
        forge,
        context.path(),
        Batch {
            identity: RepoIdentity {
                name_with_owner: own,
                id: String::new(),
            },
            requirements: ours,
            heads: Some(Heads {
                members,
                owner,
                entry: context.fork.entry,
            }),
        },
        outcomes,
    );
}

/// What a review pull request on the fork's own repository is judged by: its
/// head, when that head is a member branch of the fork.
struct Heads<'m, 'r> {
    members: &'m Members<'r>,
    /// The own repository's owner, as the forge names it now: a head of any
    /// other owner is another fork's branch, whatever its name.
    owner: String,
    entry: &'m RepoEntry,
}

impl Heads<'_, '_> {
    /// The member the pull request carries, judged at the head commit the
    /// forge reports, not at whatever a branch of its name points at here.
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
        match self.members.judge_head(&head, &fact.pull.head_ref_oid) {
            Ok(state) => state,
            Err(missing) => {
                let fetch = format!(
                    "its head commit {} is not in this checkout: fetch origin to judge it. If its \
                     branch was deleted, no fetch brings the commit back",
                    missing.short()
                );
                match fact.pull.base_ref_name.as_deref() {
                    Some(base) if fact.pull.is_merged() => MemberState::Unknown(format!(
                        "{fetch}. It merged into {base}, which holds the member's work until \
                         that lands upstream: keep this requirement until then"
                    )),
                    _ => MemberState::Unknown(fetch),
                }
            }
        }
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
        judged_commits: RefCell::new(BTreeMap::new()),
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
