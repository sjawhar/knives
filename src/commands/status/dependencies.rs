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
use crate::remote_url::{remote_slug, with_slug};
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
    /// Deleted dependents whose later work this checkout cannot read
    /// ([`dependents`]): a requirement of one that is unmet or cannot be
    /// judged is held here, for one problem per dependent; a met one says
    /// nothing.
    uncertain: &'a BTreeMap<BranchName, Unread>,
    /// Per uncertain dependent, each such requirement and what was seen of
    /// it.
    deferred: BTreeMap<BranchName, Vec<(Requirement, String)>>,
    /// Per uncertain dependent, every requirement it holds: its command
    /// names every record of a number or member it gives, as `depends`'s own
    /// recovery does, so it never needs a second step.
    held: BTreeMap<BranchName, Vec<Requirement>>,
    /// The spellings of this fork's own repository here, lowercase: a member
    /// record on any other repository is named by its own token, never by a
    /// branch name that also names this fork's member.
    own: Vec<String>,
    /// The repositories, lowercase, that are not this fork's: other forks'
    /// own repositories, and those the forge named as another repository. A
    /// record on one is never another record's ([`Self::tokens`]).
    apart: Vec<String>,
}

/// What could not be read of a deleted dependent's work.
struct Unread {
    why: String,
    /// Whether a fetch can bring it: its recorded commit is not here.
    fetchable: bool,
}

/// One requirement of one dependent, `shown` as the report names it.
#[derive(Clone, Copy)]
struct About<'r> {
    branch: &'r BranchName,
    requirement: &'r Requirement,
    shown: &'r str,
}

impl DependencyResults<'_> {
    /// A pull request required, by the `state` the forge gave it.
    fn record(&mut self, about: About<'_>, state: Option<&str>) {
        let About { branch, shown, .. } = about;
        match state {
            Some(state) if state.eq_ignore_ascii_case("MERGED") => {}
            Some(state) => self.unmet(about, &format!("which is {}", state.to_lowercase())),
            None if self.uncertain.contains_key(branch) => {
                self.defer(about, format!("{shown}, which the forge did not report on"));
            }
            None => self.problems.push(format!(
                "{branch} requires {shown}, which the forge did not report on"
            )),
        }
    }

    /// A member required, by what this checkout says of it.
    fn member(&mut self, about: About<'_>, state: MemberState) {
        match state {
            MemberState::Landed => {}
            MemberState::Open(seen) => self.unmet(about, &format!("which has not landed ({seen})")),
            MemberState::Unprobed(why) => {
                self.unmet(about, &format!("which is not known to have landed: {why}"));
            }
            MemberState::Unknown(why) => self.unchecked(about, why),
        }
    }

    fn unmet(&mut self, about: About<'_>, what: &str) {
        let About { branch, shown, .. } = about;
        if self.uncertain.contains_key(branch) {
            self.defer(about, format!("{shown}, {what}"));
            return;
        }
        self.findings.push(Finding::new(
            FindingKind::UnmetDependency,
            Subject::Branch(branch.clone()),
            format!("{branch} requires {shown}, {what}"),
        ));
    }

    /// A requirement nothing here can judge: `problem` for a dependent
    /// checked as a row's; for an uncertain one, `why` joins its one problem.
    fn not_judged(&mut self, about: About<'_>, problem: String, why: &str) {
        if self.uncertain.contains_key(about.branch) {
            self.defer(about, format!("{}, {why}", about.shown));
            return;
        }
        self.problems.push(problem);
    }

    fn unchecked(&mut self, about: About<'_>, why: impl Display) {
        let About { branch, shown, .. } = about;
        if self.uncertain.contains_key(branch) {
            self.defer(about, format!("{shown}, which could not be judged: {why}"));
            return;
        }
        self.problems.push(format!(
            "cannot check whether {branch} still needs {shown}: {why}"
        ));
    }

    /// [`Self::unchecked`] for each of `requirements`, for one reason.
    fn unchecked_each(
        &mut self,
        requirements: impl IntoIterator<Item = Wanted>,
        why: impl Display,
    ) {
        for (branch, requirement, _) in requirements {
            let shown = requirement.to_string();
            self.unchecked(
                About {
                    branch: &branch,
                    requirement: &requirement,
                    shown: &shown,
                },
                &why,
            );
        }
    }

    fn defer(&mut self, about: About<'_>, seen: String) {
        self.deferred
            .entry(about.branch.clone())
            .or_default()
            .push((about.requirement.clone(), seen));
    }

    /// One problem per uncertain dependent with a requirement that is unmet
    /// or cannot be judged: what could not be read, what was seen of each
    /// such requirement, and the command that forgets them
    /// ([`Self::tokens`]), with what else it takes back.
    fn report_deferred(&mut self) {
        for (branch, open) in std::mem::take(&mut self.deferred) {
            let Some(unread) = self.uncertain.get(&branch) else {
                continue;
            };
            let mut needs: Vec<String> = Vec::new();
            for (requirement, _) in &open {
                let shown = requirement.to_string();
                if !needs.contains(&shown) {
                    needs.push(shown);
                }
            }
            let requirements: Vec<&Requirement> =
                open.iter().map(|(requirement, _)| requirement).collect();
            let (tokens, besides) = self.tokens(&branch, &requirements);
            let seen: Vec<&str> = open.iter().map(|(_, seen)| seen.as_str()).collect();
            let besides = if besides.is_empty() {
                String::new()
            } else {
                format!(
                    "; the command takes back {} too, which may be the same requirement under \
                     another name of its repository",
                    besides.join(", ")
                )
            };
            let command = tokens.join(" --forget ");
            let next = if unread.fetchable {
                "Fetch a ref that holds its work to judge it, or forget these once its work has \
                 landed or been dropped"
            } else {
                "Forget these once its work has landed or been dropped"
            };
            self.problems.push(format!(
                "cannot tell whether {branch} still needs {}: its branch is gone here and on \
                 origin, and {}; {}{besides}. {next}: `knives depends {branch} --forget {command}`",
                needs.join(", "),
                unread.why,
                seen.join("; ")
            ));
        }
    }

    /// The `--forget` texts that take back `open`, requirements of
    /// `dependent`, in one run while the forge answers as it did for this
    /// report, and the records they take besides `open`'s. An upstream pull
    /// request, and a record on a repository that is not this fork's
    /// ([`Self::apart`]), by its own token alone: if the forge stops
    /// answering before the forget, a record it named apart may be meant
    /// again, and `depends` refuses and prints a broader command. Otherwise
    /// every record `dependent` holds of the review pull request number or
    /// member, on any repository that is not apart: one whose repository the
    /// forge could not be asked about may be the same requirement. A member
    /// by its branch name when every such record spells a spelling here and
    /// the name holds no `#`; else each record as it is spelled.
    fn tokens(&self, dependent: &BranchName, open: &[&Requirement]) -> (Vec<String>, Vec<String>) {
        let held = self.held.get(dependent).map_or(&[][..], Vec::as_slice);
        let spelled = |record: &Requirement, among: &[String]| {
            record
                .repository()
                .is_some_and(|repository| among.contains(&repository.to_ascii_lowercase()))
        };
        let mut tokens: Vec<String> = Vec::new();
        let mut besides: Vec<String> = Vec::new();
        for &requirement in open {
            let of_it: Vec<&Requirement> = if requirement.repository().is_none()
                || spelled(requirement, &self.apart)
            {
                vec![requirement]
            } else {
                held.iter()
                    .filter(|record| {
                        !spelled(record, &self.apart) && record.same_number_or_member(requirement)
                    })
                    .chain([requirement])
                    .collect()
            };
            match requirement {
                Requirement::ForkBranch { branch, .. }
                    if !branch.as_str().contains('#')
                        && of_it.iter().all(|record| spelled(record, &self.own)) =>
                {
                    if !tokens.contains(&branch.to_string()) {
                        tokens.push(branch.to_string());
                    }
                }
                _ => {
                    for record in of_it {
                        let token = record.to_string();
                        if !tokens.contains(&token) {
                            tokens.push(token.clone());
                        }
                        if !open.contains(&record) && !besides.contains(&token) {
                            besides.push(token);
                        }
                    }
                }
            }
        }
        (tokens, besides)
    }
}

/// What this checkout says of a member branch of the fork.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberState {
    /// It has landed upstream.
    Landed,
    /// It has not, with what was seen: its row's state, or the probe's verdict.
    Open(String),
    /// It has no landed row, and what alone can say it landed was not read:
    /// the landed probe did not run (`status --no-landed`, or no upstream
    /// trunk), or the forge, the only witness of a squash merge, was not
    /// asked. Kept as unmet, as the drop guard needs: a member not shown to
    /// have landed is still needed.
    Unprobed(String),
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
    /// What the forge said of this fork's upstream pull requests, the only
    /// witness of a squash merge upstream ([`Evidence`]).
    evidence: Evidence,
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
    /// holding only the origin copy judges it: both must have landed. A row
    /// with no single local tip (a conflicted bookmark) cannot say whether
    /// origin's copy is behind it, so origin's copy is judged too.
    fn judged_now(&self, member: &BranchName) -> MemberState {
        let Some(row) = self.rows.iter().find(|row| row.name == *member) else {
            return self.judge_on_origin(member);
        };
        let local = self.row_state(row);
        if local != MemberState::Landed {
            return local;
        }
        let Some(on_origin) = self.tips.get(&remote(member)) else {
            return local;
        };
        let shown = remote(member).to_string();
        let Some(tip) = self.tips.get(&BookmarkRef::Local(member.clone())) else {
            return self.judge_commit(member, on_origin, &shown);
        };
        if tip == on_origin {
            return local;
        }
        match self.repo.is_ancestor(on_origin, tip) {
            Ok(true) => local,
            Ok(false) => self.judge_commit(member, on_origin, &shown),
            Err(error) => MemberState::Unknown(format!(
                "whether {shown} holds work {member} lacks could not be read: {error}"
            )),
        }
    }

    /// What a row says of its local tip: landed when its state is landed or
    /// its landed verdict is in the trunk.
    fn row_state(&self, row: &BranchRow) -> MemberState {
        if row.state == BranchState::Landed || row.landed == Some(LandedVerdict::InTrunk) {
            MemberState::Landed
        } else if self.probed_onto.is_none() {
            MemberState::Unprobed(NO_PROBE.to_owned())
        } else {
            self.not_landed(&row.name.to_string(), || row.state.to_string())
        }
    }

    /// A member `shown` that a replay did not show landed: open, as `seen`
    /// says, when the forge answered; when it was asked and did not, it
    /// cannot be judged, since a squash merge upstream leaves nothing a
    /// replay sees; when it was not asked, not known to have landed.
    fn not_landed(&self, shown: &str, seen: impl FnOnce() -> String) -> MemberState {
        match &self.evidence {
            Evidence::Read => MemberState::Open(seen()),
            Evidence::Unanswered => MemberState::Unknown(unseen_squash(shown)),
            Evidence::NotAsked(why) => MemberState::Unprobed((*why).to_owned()),
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
            return MemberState::Unprobed(NO_PROBE.to_owned());
        };
        match crate::jj::probe_landed(self.path, &BranchName::new(tip.as_str()), trunk) {
            Ok(outcome) => match classify_landed(outcome) {
                LandedVerdict::InTrunk => MemberState::Landed,
                verdict => self.not_landed(shown, || format!("{shown} {verdict}")),
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
    /// Member records on a repository no spelling here gives, which the
    /// forge is asked to place ([`placed_members`]).
    elsewhere: Vec<(BranchName, Requirement, BranchName)>,
}

fn unmet_dependencies(
    members: &Members<'_>,
    context: &DependencyContext<'_, '_>,
    notes: &mut Vec<String>,
) -> (Vec<Finding>, Vec<String>) {
    let mut findings = Vec::new();
    let mut problems = Vec::new();
    let repo = &context.fork.upstream;
    let (checked, uncertain) = dependents(members, context, notes);
    let held = uncertain
        .keys()
        .map(|branch| {
            let target = BranchTarget::new(repo.clone(), branch.clone());
            (branch.clone(), context.store.dependencies(&target))
        })
        .collect();
    let mut outcomes = DependencyResults {
        findings: &mut findings,
        problems: &mut problems,
        uncertain: &uncertain,
        deferred: BTreeMap::new(),
        held,
        own: own_spellings(context),
        apart: other_forks_repositories(context),
    };
    let sorted = sort_requirements(repo, &checked, context, &mut outcomes);
    upstream_outcomes(repo, sorted.upstream, context, &mut outcomes);
    fork_pull_outcomes(sorted.on_fork, members, context, &mut outcomes);
    let mut judged = sorted.members;
    for placed in placed_members(sorted.elsewhere, context, &mut outcomes) {
        // One member under two spellings is one requirement.
        if !judged
            .iter()
            .any(|(dependent, _, member)| *dependent == placed.0 && *member == placed.2)
        {
            judged.push(placed);
        }
    }
    for (dependent, requirement, member) in judged {
        let shown = requirement.to_string();
        let about = About {
            branch: &dependent,
            requirement: &requirement,
            shown: &shown,
        };
        outcomes.member(about, members.judge(&member));
    }
    outcomes.report_deferred();
    (findings, problems)
}

/// Every branch whose requirements a report checks, with the deleted ones
/// whose later work cannot be read: each row; each branch with a stated
/// requirement that this checkout holds only as `<branch>@origin`, another
/// machine's branch that nobody here claims; and each one held nowhere,
/// deleted. A deleted dependent is judged by its newest recorded commit
/// ([`crate::statement::Statements::recorded_head`]): here and not landed,
/// its requirements are checked as any branch's, with a note, since a
/// branch merged into a review base before it was deleted still carries its
/// work there.
///
/// The recorded commit is a lower bound on a deleted dependent's work, not
/// its final head: the branch can have moved on with nothing written. So
/// one whose recorded commit landed, is not here, or was never recorded is
/// not taken as done: its requirements are judged too, and each that is
/// unmet or cannot be judged is reported as a problem naming what could not
/// be read ([`DependencyResults::report_deferred`]). One whose requirements
/// are all met says nothing: what it carried later cannot change a met one.
fn dependents(
    members: &Members<'_>,
    context: &DependencyContext<'_, '_>,
    notes: &mut Vec<String>,
) -> (Vec<BranchName>, BTreeMap<BranchName, Unread>) {
    let (repo, store) = (&context.fork.upstream, context.store);
    let mut dependents: Vec<BranchName> = members.rows.iter().map(|row| row.name.clone()).collect();
    let mut uncertain = BTreeMap::new();
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
        let unread = match head {
            None => Unread {
                why: "the ledger records no commit for it".to_owned(),
                fetchable: false,
            },
            Some(head) => match members.repo.resolve_commit(head.as_str()) {
                Err(_) => Unread {
                    why: format!(
                        "the newest commit the ledger records for it, {}, is not in this checkout",
                        head.short()
                    ),
                    fetchable: true,
                },
                Ok(commit) => {
                    let shown = format!("{branch} at {}", commit.short());
                    let state = members.judge_commit(&branch, &commit, &shown);
                    if state != MemberState::Landed {
                        let seen = match state {
                            MemberState::Open(_) => "has not landed".to_owned(),
                            MemberState::Unprobed(why) | MemberState::Unknown(why) => {
                                format!("could not be shown landed: {why}")
                            }
                            MemberState::Landed => String::new(),
                        };
                        notes.push(format!(
                            "{branch} is gone here and on origin; its requirements are checked \
                             because the newest commit the ledger records for it, {}, {seen}",
                            commit.short()
                        ));
                        dependents.push(branch);
                        continue;
                    }
                    Unread {
                        why: format!(
                            "the newest commit the ledger records for it, {}, has landed, which \
                             does not show that work it carried later did",
                            commit.short()
                        ),
                        fetchable: false,
                    }
                }
            },
        };
        let _ = uncertain.insert(branch.clone(), unread);
        dependents.push(branch);
    }
    (dependents, uncertain)
}

fn sort_requirements<'a>(
    repo: &UpstreamName,
    checked: &[BranchName],
    context: &DependencyContext<'a, '_>,
    outcomes: &mut DependencyResults<'_>,
) -> Sorted<'a> {
    let mut sorted = Sorted::default();
    let mut member_pairs = BTreeSet::new();
    for name in checked {
        let target = BranchTarget::new(repo.clone(), name.clone());
        for requirement in context.store.dependencies(&target) {
            let dependent = name.clone();
            let shown = requirement.to_string();
            let about = About {
                branch: &dependent,
                requirement: &requirement,
                shown: &shown,
            };
            match &requirement {
                Requirement::Upstream {
                    repo: required,
                    number,
                } => {
                    let (required, number) = (required.clone(), *number);
                    if let Some(entry) = context.registry.kept_under(&required) {
                        sorted
                            .upstream
                            .entry(required)
                            .or_insert_with(|| (entry, Vec::new()))
                            .1
                            .push((dependent, requirement, number));
                        continue;
                    }
                    outcomes.not_judged(
                        about,
                        format!(
                            "{dependent} requires {requirement}, but no fork in the registry is \
                             kept under {required}: a requirement names its fork by its \
                             upstream repository's <owner>/<name>"
                        ),
                        &format!("which names no fork the registry keeps under {required}"),
                    );
                }
                Requirement::ForkPull { repository, number } => {
                    let (repository, number) = (repository.clone(), *number);
                    sorted.on_fork.entry(repository).or_default().push((
                        dependent,
                        requirement,
                        number,
                    ));
                }
                Requirement::ForkBranch { repository, branch } => {
                    // A branch of another fork's own repository, which only a
                    // knives before this one or a hand edit writes, is no member
                    // of this fork, whatever a branch of its name here says.
                    if let Some(key) = another_forks(context, repository) {
                        outcomes.not_judged(
                            about,
                            format!(
                                "{dependent} requires {requirement}, a branch of {key}'s own \
                                 repository: no member of {}, so nothing here can judge it; \
                                 forget it and name the upstream pull request that branch becomes",
                                context.fork.name
                            ),
                            &format!(
                                "a branch of {key}'s own repository, which nothing here can judge"
                            ),
                        );
                        continue;
                    }
                    let branch = branch.clone();
                    if !outcomes.own.contains(&repository.to_ascii_lowercase()) {
                        sorted.elsewhere.push((dependent, requirement, branch));
                        continue;
                    }
                    if member_pairs.insert((dependent.clone(), branch.clone())) {
                        sorted.members.push((dependent, requirement, branch));
                    }
                }
            }
        }
    }
    sorted
}

/// Why a member not shown landed cannot be judged when the forge did not
/// answer: a squash merge upstream leaves no trace a replay can see.
fn unseen_squash(shown: &str) -> String {
    format!(
        "{shown} is not shown landed, and the forge did not answer for this fork's upstream pull \
         requests, so whether it landed by a squash merge cannot be read"
    )
}

/// Why a member not shown landed is not known to have landed when the
/// landed probe did not run.
const NO_PROBE: &str = "the landed probe did not run";

/// What the forge said of this fork's upstream pull requests, the only
/// witness of a squash merge upstream.
enum Evidence {
    /// It answered: a member not shown landed is open.
    Read,
    /// It was asked and did not answer: one not shown landed cannot be
    /// judged.
    Unanswered,
    /// It was not asked, and why: one not shown landed is not known to have
    /// landed, an unmet finding as without the landed probe.
    NotAsked(&'static str),
}

/// The spellings of this fork's own repository here, lowercase: what its
/// checkout's `origin` (or registry `origin`) names, and its registry
/// `origin`.
fn own_spellings(context: &DependencyContext<'_, '_>) -> Vec<String> {
    let mut own: Vec<String> = Vec::new();
    for url in [
        context.fork.own_origin(),
        context.fork.entry.origin.as_str(),
    ] {
        if let Some(slug) = remote_slug(url).map(str::to_ascii_lowercase)
            && !own.contains(&slug)
        {
            own.push(slug);
        }
    }
    own
}

/// The member records on a repository no spelling here gives, by the
/// dependent and the branch, that the forge names as this fork's own
/// repository (a former name nothing here spells): those are judged as
/// members. One the forge names as another repository, or no forge
/// consulted or able to answer, is a requirement nothing here can judge.
fn placed_members(
    elsewhere: Vec<(BranchName, Requirement, BranchName)>,
    context: &DependencyContext<'_, '_>,
    outcomes: &mut DependencyResults<'_>,
) -> Vec<(BranchName, Requirement, BranchName)> {
    let mut placed = Vec::new();
    if elsewhere.is_empty() {
        return placed;
    }
    let fork = &context.fork.name;
    let origin = context.fork.own_origin();
    let named = |url: &str| -> Result<String, String> {
        let Some(forge) = context.forge else {
            return Err("no forge consulted".to_owned());
        };
        forge
            .repo_identity(context.path(), url, url)
            .map(|identity| identity.name_with_owner.to_ascii_lowercase())
            .map_err(|error| error.to_string())
    };
    let own = named(origin);
    for (dependent, requirement, branch) in elsewhere {
        let Requirement::ForkBranch { repository, .. } = &requirement else {
            continue;
        };
        let resolved = own.clone().and_then(|own| {
            let url = with_slug(origin, repository)
                .ok_or_else(|| format!("{fork}'s {ORIGIN} names no forge repository"))?;
            named(&url).map(|name| name == own)
        });
        let shown = requirement.to_string();
        let about = About {
            branch: &dependent,
            requirement: &requirement,
            shown: &shown,
        };
        match resolved {
            Ok(true) => {
                placed.push((dependent.clone(), requirement.clone(), branch));
            }
            Ok(false) => {
                outcomes.apart.push(repository.to_ascii_lowercase());
                outcomes.not_judged(
                about,
                format!(
                    "{dependent} requires {requirement}, a branch of {repository}, which is not \
                     {fork}'s own repository: no member of {fork}, so nothing here can judge it"
                ),
                &format!("a branch of {repository}, which is not {fork}'s own repository"),
                );
            }
            Err(error) => outcomes.unchecked(
                about,
                format_args!(
                    "whether {repository} is {fork}'s own repository could not be asked: {error}"
                ),
            ),
        }
    }
    placed
}

/// The own repositories of the forks other than this one, lowercase, as the
/// registry spells them.
fn other_forks_repositories(context: &DependencyContext<'_, '_>) -> Vec<String> {
    context
        .registry
        .repos
        .iter()
        .filter(|(key, entry)| entry.upstream_name(key) != context.fork.upstream)
        .filter_map(|(_, entry)| remote_slug(&entry.origin).map(str::to_ascii_lowercase))
        .collect()
}

/// The registry name of a fork other than this one whose own repository the
/// registry spells `repository`.
fn another_forks<'r>(context: &DependencyContext<'r, '_>, repository: &str) -> Option<&'r str> {
    context
        .registry
        .repos
        .iter()
        .find(|(key, entry)| {
            entry.upstream_name(key) != context.fork.upstream
                && remote_slug(&entry.origin)
                    .is_some_and(|slug| slug.eq_ignore_ascii_case(repository))
        })
        .map(|(key, _)| key.as_str())
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
                let why = if context.forge.is_some() {
                    "the forge did not answer for this fork's upstream pull requests"
                } else {
                    "no forge consulted"
                };
                outcomes.unchecked_each(requirements, why);
                continue;
            };
            for (branch, requirement, number) in requirements {
                let shown = requirement.to_string();
                outcomes.record(
                    About {
                        branch: &branch,
                        requirement: &requirement,
                        shown: &shown,
                    },
                    snapshot.fact(number).map(|fact| fact.pull.state.as_str()),
                );
            }
            continue;
        }

        let Some(forge) = context.forge else {
            outcomes.unchecked_each(requirements, "no forge consulted");
            continue;
        };
        let Some(identity) = upstream_identity(entry) else {
            for (branch, requirement, _) in requirements {
                let shown = requirement.to_string();
                outcomes.not_judged(
                    About {
                        branch: &branch,
                        requirement: &requirement,
                        shown: &shown,
                    },
                    format!(
                        "{branch} requires {requirement}, but {required_repo}'s upstream {} is \
                         not a forge repository: cannot check dependencies against it",
                        entry.upstream
                    ),
                    &format!(
                        "on {required_repo}'s upstream {}, which is not a forge repository",
                        entry.upstream
                    ),
                );
            }
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
    let origin = context.fork.own_origin();
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
            with_slug(origin, &repository)
                .map_or(Ok(false), |url| named(url.as_str()).map(|name| name == own))
        };
        match resolved {
            Ok(true) => ours.extend(requirements),
            Ok(false) => {
                outcomes.apart.push(repository.to_ascii_lowercase());
                outcomes.unchecked_each(
                    requirements,
                    format_args!(
                        "{repository} is not {fork}'s own repository, and a review pull request on \
                     another fork's repository cannot be judged here; forget it and name the \
                     upstream pull request its member becomes"
                    ),
                );
            }
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
                     branch was deleted, fetch a ref that still holds the commit: the base it \
                     merged into, or the pull request's own `refs/pull/{}/head`",
                    missing.short(),
                    fact.pull.number
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
                let fact = facts.get(&number);
                let shown = match (fact, heads.as_ref()) {
                    (Some(fact), Some(_)) => format!(
                        "{requirement} (head {}, {})",
                        fact.pull.head_ref_name,
                        fact.pull.state.to_lowercase()
                    ),
                    _ => requirement.to_string(),
                };
                let about = About {
                    branch: &branch,
                    requirement: &requirement,
                    shown: &shown,
                };
                match (fact, heads.as_ref()) {
                    (Some(fact), Some(heads)) => outcomes.member(about, heads.judge(fact)),
                    (fact, _) => {
                        outcomes.record(about, fact.map(|fact| fact.pull.state.as_str()));
                    }
                }
            }
        }
        Err(error) => outcomes.unchecked_each(requirements, &error),
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
        evidence: if post.options.forge.is_none() {
            Evidence::NotAsked(
                "the forge was not asked (--no-github), so a squash merge cannot be seen",
            )
        } else if remote_slug(&post.fork.entry.upstream).is_none() {
            Evidence::NotAsked(
                "the fork's upstream is not a forge repository, so a squash merge cannot be seen",
            )
        } else if snapshot.is_some() {
            Evidence::Read
        } else {
            Evidence::Unanswered
        },
        judged: RefCell::new(BTreeMap::new()),
        judged_commits: RefCell::new(BTreeMap::new()),
    };
    let mut notes = Vec::new();
    let (found, unanswered) = unmet_dependencies(
        &members,
        &DependencyContext {
            store: post.store,
            registry,
            fork: post.fork,
            forge: post.options.forge,
            snapshot,
        },
        &mut notes,
    );
    timings.forge += started.elapsed();
    findings.extend(found);
    report.problems.extend(unanswered);
    report.notes.extend(notes);
}
