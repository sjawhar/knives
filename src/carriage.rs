//! Whether content is actually carried — by replay and ancestry, never text.
//!
//! The audit's worst near-miss class was a branch deleted while its content
//! was uncarried. The verdicts here are content-based first: sha ancestry for
//! carried-exact (for a release, ancestry and every line of its own change in
//! the release as it left it), a three-way tree merge for carried-rewritten (jj
//! divergent change-ids force tree comparison — the same change id can name
//! two different trees). Only when the tree says otherwise does the change id
//! speak, and it says something weaker on purpose: carried-rebased means a
//! rewrite of the change (`jj rebase` keeps the change id) is in the target's
//! ancestry, so the change lives on there in the form its author gave it on
//! the new base — not that this exact tree does. Merge conflicts with no such
//! rewrite stay a matter for human judgment. A net-zero revision is vacuously
//! carried: it contributes no content for the target to lack. Every verdict
//! names an evidence commit a notch can cite and a later reader can
//! re-resolve.

use std::collections::BTreeMap;
use std::path::Path;

use crate::bind::Fork;
use crate::config::Role;
use crate::detect::{BookmarkTips, RebaseOutcome};
use crate::forge::{Forge, index_pulls};
use crate::ids::{BookmarkRef, BranchName, CommitId, CutKey, ReleaseScheme, is_our_release};
use crate::jj::Repo;
use crate::release_model;
use crate::snapshot::{self, SnapshotConfig};

/// What a revision is checked against.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Target {
    /// Every ref naming this commit — `release/X`, `release/X@origin`, … —
    /// so a double-cut shows up as two targets with one name.
    pub refs: Vec<BookmarkRef>,
    pub commit: CommitId,
    pub role: TargetRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetRole {
    /// A ref of the newest release name (or the fixed release branch).
    LiveRelease,
    /// A ref of an older dated name that still exists somewhere ours.
    SupersededRelease,
    /// The upstream trunk.
    UpstreamTrunk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CarryVerdict {
    /// The revision's tip is an ancestor of the target: carried as-is. For a
    /// release target, the release also holds every line of the tip's own
    /// change as the tip left it: a release merge reaches every member it
    /// names as a parent whatever its tree holds, so ancestry alone does not
    /// show a hand resolution kept it ([`check`]).
    CarriedExact,
    /// Its net tree change leaves the target unchanged, whether the same content
    /// arrived through different commits or the revision itself has no net content.
    CarriedRewritten,
    /// Its tree does not replay cleanly, but a rewrite of its change that
    /// stands for it — a successor of it, or a commit with its tree
    /// ([`crate::jj::Repo::standing_rewrites_of`]) — is an ancestor of the
    /// target: the change is carried as its author rebased it, and that
    /// rewrite is the evidence. An older version of the change, which only
    /// shares its change id, carries nothing of what the newer one added.
    CarriedRebased,
    NotCarried,
    /// The replay conflicted while the target itself is clean, and, for a
    /// tip the target reaches, some line of its own change is not in the
    /// target as it left it: some content is there or unrelated work touched
    /// the same files; judge by eye.
    Conflicted,
}

impl CarryVerdict {
    pub const fn carried(self) -> bool {
        matches!(
            self,
            Self::CarriedExact | Self::CarriedRewritten | Self::CarriedRebased
        )
    }
}

/// One verdict with the commit that proves it.
///
/// The revision tip for carried-exact (it IS in the target's ancestry, and
/// the target holds its change), the rewrite for carried-rebased (the same
/// change, in the target's ancestry), the target commit otherwise (the tree
/// the replay was judged against).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CarryCheck {
    pub verdict: CarryVerdict,
    pub evidence: CommitId,
}
/// One target's carriage verdict, including how it was classified for exit
/// semantics and the commit that establishes the verdict.
#[derive(Debug, serde::Serialize)]
pub struct TargetCheck {
    /// Ref names, `/`-joined for display; a requested revision when no ref names it.
    pub target: String,
    pub commit: CommitId,
    pub role: TargetRole,
    pub verdict: CarryVerdict,
    pub evidence: CommitId,
}

/// The complete answer to a `release members --carries` query.
#[derive(Debug, serde::Serialize)]
pub struct CarriesReport {
    pub repo: String,
    pub revision: String,
    pub checks: Vec<TargetCheck>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

/// One maintained branch in the census.
#[derive(Debug, serde::Serialize)]
pub struct BranchCarriage {
    pub branch: String,
    pub tip: CommitId,
    pub checks: Vec<TargetCheck>,
    /// `Some(true)` means an open pull request carries this branch's content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_open_pull: Option<bool>,
    /// `Some(true)` is a proven content orphan; `Some(false)` is not an
    /// orphan. `None` means a pull request or carriage check was unavailable,
    /// so consumers must not read the row as a deletion-safe claim.
    pub orphan: Option<bool>,
}

/// The carriage census over maintained branches.
#[derive(Debug, serde::Serialize)]
pub struct CensusReport {
    pub repo: String,
    pub rows: Vec<BranchCarriage>,
    pub orphans: Vec<String>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

/// Optional resources and limits for a carriage census.
#[derive(Debug, Clone, Copy)]
pub struct CensusOptions<'a> {
    pub cache_root: Option<&'a Path>,
    pub workers: usize,
}

/// Census maintained branches against live releases and the upstream trunk.
///
/// It escalates only a complete, everywhere-not-carried primary matrix to
/// superseded releases. Member checks run in bounded, input-ordered worker
/// chunks, each with its own repository handle.
///
/// The live `ai` census fell from 46m42.510s over 217 members to 11.785s over
/// 26 maintained branches after anonymous-head removal and bounded parallelism.
///
/// `forge: None` leaves pull-request state explicitly unknown; carriage itself
/// remains a local repository question and still completes.
pub fn census(
    fork: &Fork<'_>,
    forge: Option<&dyn Forge>,
    options: CensusOptions<'_>,
    records: &Records<'_>,
) -> anyhow::Result<CensusReport> {
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let repo = Repo::open(path)?;
    let trunk_name = entry.trunk_ref();
    let trunk = repo.resolve_commit(&trunk_name)?;
    let tips = repo.bookmark_tips()?;
    let trunk_branch = entry.trunk();
    let branches: Vec<(BranchName, CommitId)> =
        release_model::carried_from_tips(&tips, trunk_branch, &entry.release_scheme())
            .into_iter()
            .map(|(branch, tip)| (BranchName::new(branch), tip))
            .collect();
    let all_targets = targets(
        &tips,
        &entry.release_scheme(),
        (trunk_name.as_str(), trunk.clone()),
        entry.publish_remote(),
    );
    let (primary, superseded): (Vec<Target>, Vec<Target>) = all_targets
        .into_iter()
        .partition(|target| target.role != TargetRole::SupersededRelease);
    let mut notes = vec![format!(
        "primary targets: {}",
        primary
            .iter()
            .map(|target| target_name(target, trunk_name.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    )];
    let CensusPulls {
        branch_pulls,
        checked: pull_requests_checked,
        notes: pull_notes,
    } = census_pulls(forge, fork, branches.as_slice(), options.cache_root);
    notes.extend(pull_notes);

    let own = own_repository_spellings(fork);
    let context = BaseContext {
        trunk: &trunk,
        own: &own,
        branches: &branches,
        pull_head: records.pull_head,
    };
    let mut members = Vec::with_capacity(branches.len());
    for (branch, tip) in &branches {
        let recorded = (records.requirements)(branch.as_str());
        let base = MemberBase::read(
            &repo,
            (Some(branch.as_str()), tip),
            recorded.as_deref().map_err(String::as_str),
            &context,
        )?;
        members.push(CensusInput {
            in_open_pull: pull_requests_checked
                .then(|| branch_pulls.get(branch).copied().unwrap_or(false)),
            branch: branch.to_string(),
            tip: tip.clone(),
            base,
        });
    }
    let targets = CensusTargets {
        primary: &primary,
        superseded: &superseded,
        fallback: trunk_name.as_str(),
        trunk: &trunk,
    };
    let (members, problems) = census_rows(path, &members, targets, options.workers);
    let orphans = members
        .iter()
        .filter(|member| member.list_as_orphan)
        .map(|member| member.carriage.branch.clone())
        .collect();
    let rows = members.into_iter().map(|member| member.carriage).collect();
    Ok(CensusReport {
        repo: fork.name.to_string(),
        rows,
        orphans,
        notes,
        problems,
    })
}

fn select_census_numbers(
    discovery: &snapshot::Discovery<'_>,
    branches: &[(BranchName, CommitId)],
) -> Vec<u64> {
    let index = index_pulls(&discovery.ours());
    branches
        .iter()
        .filter_map(|(name, _)| index.by_branch.get(name).map(|pull| pull.number))
        .collect()
}

struct CensusPulls {
    branch_pulls: BTreeMap<BranchName, bool>,
    checked: bool,
    notes: Vec<String>,
}

fn census_pulls(
    forge: Option<&dyn Forge>,
    fork: &Fork<'_>,
    branches: &[(BranchName, CommitId)],
    cache_root: Option<&Path>,
) -> CensusPulls {
    let entry = fork.entry;
    let Some(forge) = forge else {
        return CensusPulls {
            branch_pulls: BTreeMap::new(),
            checked: false,
            notes: vec!["pull request state was not checked".to_owned()],
        };
    };
    let mut pulls = CensusPulls {
        branch_pulls: BTreeMap::new(),
        checked: false,
        notes: Vec::new(),
    };
    let opened = match snapshot::open(SnapshotConfig {
        forge,
        path: &fork.checkout.path,
        upstream: entry.remote(Role::Upstream),
        remotes: [entry.remote(Role::Origin), entry.remote(Role::Release)],
        cache_root,
    }) {
        Ok(opened) => opened,
        Err(error) => {
            pulls
                .notes
                .push(format!("pull request state unavailable: {error}"));
            return pulls;
        }
    };
    match opened.complete_with(branches, select_census_numbers) {
        Ok(snapshot) => {
            pulls.checked = snapshot
                .requested()
                .iter()
                .all(|number| snapshot.fact(*number).is_some());
            pulls.branch_pulls = branches
                .iter()
                .filter_map(|(branch, _)| {
                    snapshot.index().by_branch.get(branch).and_then(|pull| {
                        snapshot
                            .fact(pull.number)
                            .map(|fact| (branch.clone(), fact.pull.is_open()))
                    })
                })
                .collect();
            if let Err(error) = snapshot.persist(None) {
                pulls.notes.push(error.to_string());
            }
        }
        Err(error) => pulls
            .notes
            .push(format!("pull request state unavailable: {error}")),
    }
    pulls
}

struct CensusInput {
    branch: String,
    tip: CommitId,
    base: MemberBase,
    in_open_pull: Option<bool>,
}

struct CensusMember {
    carriage: BranchCarriage,
    list_as_orphan: bool,
}

#[derive(Clone, Copy)]
struct CensusTargets<'a> {
    primary: &'a [Target],
    superseded: &'a [Target],
    fallback: &'a str,
    /// The upstream trunk a release target's replay takes a member's fork
    /// point with ([`CheckInput::trunk`]).
    trunk: &'a CommitId,
}

struct CensusChunk {
    members: Vec<CensusMember>,
    problems: Vec<String>,
}

impl CensusChunk {
    fn panicked(inputs: &[CensusInput]) -> Self {
        Self {
            members: inputs.iter().map(unanswered_census_member).collect(),
            problems: inputs
                .iter()
                .map(|input| format!("census check task panicked for {}", input.branch))
                .collect(),
        }
    }
}

/// Every maintained member is checked in branch order, with no more than
/// `workers` repository handles open at once.
fn census_rows(
    repo_path: &Path,
    inputs: &[CensusInput],
    targets: CensusTargets<'_>,
    workers: usize,
) -> (Vec<CensusMember>, Vec<String>) {
    if inputs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let workers = workers.clamp(1, inputs.len());
    let chunk = inputs.len().div_ceil(workers);
    let chunks = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for slice in inputs.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || match Repo::open(repo_path) {
                    Ok(repo) => {
                        let mut problems = Vec::new();
                        let members = slice
                            .iter()
                            .map(|input| {
                                let check_input = CheckInput::new(
                                    &repo,
                                    &input.branch,
                                    (&input.tip, input.base.clone()),
                                    targets.trunk,
                                );
                                census_member(input, targets, &mut problems, |target| {
                                    check(&check_input, target)
                                })
                            })
                            .collect();
                        CensusChunk { members, problems }
                    }
                    Err(error) => {
                        let error = error.to_string();
                        CensusChunk {
                            members: slice.iter().map(unanswered_census_member).collect(),
                            problems: slice
                                .iter()
                                .map(|input| {
                                    format!(
                                        "cannot open repository for census check {}: {error}",
                                        input.branch
                                    )
                                })
                                .collect(),
                        }
                    }
                }),
            ));
        }
        handles
            .into_iter()
            .map(|(slice, handle)| {
                handle
                    .join()
                    .unwrap_or_else(|_| CensusChunk::panicked(slice))
            })
            .collect::<Vec<_>>()
    });
    let mut members = Vec::with_capacity(inputs.len());
    let mut problems = Vec::new();
    for mut chunk in chunks {
        members.append(&mut chunk.members);
        problems.append(&mut chunk.problems);
    }
    (members, problems)
}

fn census_member<F>(
    input: &CensusInput,
    targets: CensusTargets<'_>,
    problems: &mut Vec<String>,
    mut check_target: F,
) -> CensusMember
where
    F: FnMut(&Target) -> anyhow::Result<CarryCheck>,
{
    let mut checks = CensusChecks::new(
        &input.branch,
        problems,
        targets.primary.len() + targets.superseded.len(),
    );
    let primary_complete = checks.append(targets.primary, targets.fallback, &mut check_target);
    let escalation_complete = if primary_complete
        && checks
            .entries
            .iter()
            .all(|check| check.verdict == CarryVerdict::NotCarried)
    {
        checks.append(targets.superseded, targets.fallback, &mut check_target)
    } else {
        true
    };
    let orphan = orphan_status(
        primary_complete,
        escalation_complete,
        checks.entries.iter().map(|check| check.verdict),
        input.in_open_pull,
    );
    CensusMember {
        carriage: BranchCarriage {
            branch: input.branch.clone(),
            tip: input.tip.clone(),
            checks: checks.entries,
            in_open_pull: input.in_open_pull,
            orphan: orphan.status,
        },
        list_as_orphan: orphan.list_as_orphan,
    }
}

struct CensusChecks<'a> {
    branch: &'a str,
    entries: Vec<TargetCheck>,
    problems: &'a mut Vec<String>,
}

impl<'a> CensusChecks<'a> {
    fn new(branch: &'a str, problems: &'a mut Vec<String>, capacity: usize) -> Self {
        Self {
            branch,
            entries: Vec::with_capacity(capacity),
            problems,
        }
    }

    fn append<F>(&mut self, targets: &[Target], fallback: &str, check_target: &mut F) -> bool
    where
        F: FnMut(&Target) -> anyhow::Result<CarryCheck>,
    {
        let mut complete = true;
        for target in targets {
            let name = target_name(target, fallback);
            match check_target(target) {
                Ok(check) => self.entries.push(TargetCheck {
                    target: name,
                    commit: target.commit.clone(),
                    role: target.role,
                    verdict: check.verdict,
                    evidence: check.evidence,
                }),
                Err(error) => {
                    complete = false;
                    self.problems.push(format!(
                        "cannot check {} against {name}: {error}",
                        self.branch
                    ));
                }
            }
        }
        complete
    }
}

fn unanswered_census_member(input: &CensusInput) -> CensusMember {
    CensusMember {
        carriage: BranchCarriage {
            branch: input.branch.clone(),
            tip: input.tip.clone(),
            checks: Vec::new(),
            in_open_pull: input.in_open_pull,
            orphan: None,
        },
        list_as_orphan: false,
    }
}

struct OrphanStatus {
    status: Option<bool>,
    list_as_orphan: bool,
}

fn orphan_status(
    primary_complete: bool,
    escalation_complete: bool,
    mut checks: impl Iterator<Item = CarryVerdict>,
    in_open_pull: Option<bool>,
) -> OrphanStatus {
    if !primary_complete || !escalation_complete {
        return OrphanStatus {
            status: None,
            list_as_orphan: false,
        };
    }
    if !checks.all(|verdict| verdict == CarryVerdict::NotCarried) {
        return OrphanStatus {
            status: Some(false),
            list_as_orphan: false,
        };
    }
    let status = in_open_pull.map(|open| !open);
    OrphanStatus {
        status,
        list_as_orphan: status != Some(false),
    }
}

/// The durable label for a target, including every ref at its commit.
pub fn target_name(target: &Target, fallback: &str) -> String {
    let name = target
        .refs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("/");
    if name.is_empty() {
        fallback.to_owned()
    } else {
        name
    }
}

/// Render the human-readable branch-and-commit census.
///
/// A branch, an orphan and a check target are names read from the
/// repository's refs, which a remote can give a C1 character, and a note and
/// a problem quote jj and the forge, so all of them reach the terminal
/// escaped; the repository is a registry key.
pub fn render_census(report: &CensusReport) -> String {
    let mut lines = vec![format!("{}: census", report.repo)];
    let mut notes = report.notes.iter();
    if let Some(primary) = notes.next() {
        lines.push(crate::ledger::inline_human_text(primary));
    }
    for row in &report.rows {
        lines.push(String::new());
        render_census_row(&mut lines, row);
    }
    lines.push(String::new());
    let pull_state_unknown = report.notes.iter().any(|note| {
        note == "pull request state was not checked"
            || note.starts_with("pull request state unavailable:")
    });
    if pull_state_unknown {
        lines.push("orphans: not carried anywhere (pull request state unknown)".to_owned());
    } else {
        lines.push("orphans:".to_owned());
    }
    if report.orphans.is_empty() {
        lines.push("  none".to_owned());
    } else {
        lines.extend(
            report
                .orphans
                .iter()
                .map(|orphan| format!("  {}", crate::ledger::inline_human_text(orphan))),
        );
    }
    lines.extend(notes.map(|note| format!("note: {}", crate::ledger::inline_human_text(note))));
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("unanswered: {}", crate::ledger::problem_human_text(problem))),
    );
    lines.join("\n")
}

fn render_census_row(lines: &mut Vec<String>, row: &BranchCarriage) {
    lines.push(format!(
        "  {} @ {}",
        crate::ledger::inline_human_text(&row.branch),
        row.tip.short()
    ));
    lines.extend(row.checks.iter().map(|check| render_check(check, "    ")));
    let pull = match row.in_open_pull {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    lines.push(format!("    open pull request: {pull}"));
}

/// Render the human-readable form of a multi-target carriage report.
///
/// The revision is the caller's own argument; a check target is a name read
/// from the repository's refs, and a note and a problem quote jj and the
/// forge, so those reach the terminal escaped.
pub fn render_carries(report: &CarriesReport) -> String {
    let mut lines = vec![format!("{}: {}", report.repo, report.revision)];
    lines.extend(report.checks.iter().map(|check| render_check(check, "  ")));
    lines.extend(
        report
            .notes
            .iter()
            .map(|note| format!("  note: {}", crate::ledger::inline_human_text(note))),
    );
    lines.extend(report.problems.iter().map(|problem| {
        format!(
            "  unanswered: {}",
            crate::ledger::problem_human_text(problem)
        )
    }));
    lines.join("\n")
}

fn render_check(check: &TargetCheck, indent: &str) -> String {
    let verdict = match check.verdict {
        CarryVerdict::CarriedExact => "carried-exact",
        CarryVerdict::CarriedRewritten => "carried-rewritten",
        CarryVerdict::CarriedRebased => "carried-rebased",
        CarryVerdict::NotCarried => "NOT carried",
        CarryVerdict::Conflicted => "conflicted",
    };
    let reason = match (check.verdict, check.role) {
        (CarryVerdict::CarriedExact, TargetRole::UpstreamTrunk) => "tip is an ancestor",
        (CarryVerdict::CarriedExact, TargetRole::LiveRelease | TargetRole::SupersededRelease) => {
            "tip is an ancestor and the release holds every line of its own change"
        }
        (CarryVerdict::CarriedRewritten, _) => "no net content remains",
        (CarryVerdict::CarriedRebased, _) => "a rewrite of the change is an ancestor",
        (CarryVerdict::NotCarried, _) => "replay leaves real diffs",
        (CarryVerdict::Conflicted, _) => "judge by eye",
    };
    let target = if check.target.is_empty() {
        match check.role {
            TargetRole::UpstreamTrunk => "upstream trunk".to_owned(),
            TargetRole::LiveRelease | TargetRole::SupersededRelease => "unnamed release".to_owned(),
        }
    } else {
        crate::ledger::inline_human_text(&check.target)
    };
    format!(
        "{indent}{verdict:<19}{target} @ {}  (evidence {}: {reason})",
        check.commit.short(),
        check.evidence.short(),
    )
}

/// Every check target for this repository: each distinct commit named by our
/// release refs (grouped, so one name at two commits is two targets), plus the
/// upstream trunk.
///
/// Live/superseded comes from [`ReleaseScheme::cut_key`] ordering within the
/// scheme's series; under a fixed scheme every release ref of the configured
/// name is Live.
pub fn targets(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    trunk: (&str, CommitId),
    publish_remote: &str,
) -> Vec<Target> {
    let newest_release = match scheme {
        ReleaseScheme::Dated | ReleaseScheme::Series(_) => tips
            .keys()
            .filter(|reference| is_our_release(reference, scheme, publish_remote))
            .filter_map(|reference| scheme.cut_key(reference.branch().as_str()))
            .max(),
        ReleaseScheme::Fixed(_) => None,
    };
    let mut grouped =
        BTreeMap::<CommitId, (Vec<BookmarkRef>, TargetRole, Option<CutKey<'_>>)>::new();

    for (reference, commit) in tips
        .iter()
        .filter(|(reference, _)| is_our_release(reference, scheme, publish_remote))
    {
        let dated_name = scheme.cut_key(reference.branch().as_str());
        let role = match scheme {
            ReleaseScheme::Dated | ReleaseScheme::Series(_)
                if matches!(
                    (dated_name, newest_release),
                    (Some(dated_name), Some(newest_release)) if dated_name == newest_release
                ) =>
            {
                TargetRole::LiveRelease
            }
            ReleaseScheme::Dated | ReleaseScheme::Series(_) => TargetRole::SupersededRelease,
            ReleaseScheme::Fixed(_) => TargetRole::LiveRelease,
        };
        let entry = grouped.entry(commit.clone()).or_insert((
            Vec::new(),
            TargetRole::SupersededRelease,
            dated_name,
        ));
        entry.0.push(reference.clone());
        if role == TargetRole::LiveRelease {
            entry.1 = TargetRole::LiveRelease;
        }
        if dated_name > entry.2 {
            entry.2 = dated_name;
        }
    }

    let mut live = Vec::new();
    let mut superseded = Vec::new();
    for (commit, (refs, role, newest_name)) in grouped {
        let target = Target { refs, commit, role };
        if role == TargetRole::LiveRelease {
            live.push(target);
        } else {
            superseded.push((newest_name, target));
        }
    }
    superseded.sort_by(|(left_name, left_target), (right_name, right_target)| {
        right_name
            .cmp(left_name)
            .then_with(|| left_target.commit.cmp(&right_target.commit))
    });

    let trunk_refs = tips
        .iter()
        .filter_map(|(reference, commit)| {
            let names_trunk = match reference {
                BookmarkRef::Local(branch) => branch.as_str() == trunk.0,
                BookmarkRef::Remote { branch, remote } => {
                    trunk
                        .0
                        .strip_suffix(remote.as_str())
                        .and_then(|prefix| prefix.strip_suffix('@'))
                        == Some(branch.as_str())
                }
            };
            (names_trunk && commit == &trunk.1).then(|| reference.clone())
        })
        .collect();

    live.push(Target {
        refs: trunk_refs,
        commit: trunk.1,
        role: TargetRole::UpstreamTrunk,
    });
    live.extend(superseded.into_iter().map(|(_, target)| target));
    live
}

/// What [`MemberBase::read`] is told by the caller: what the ledger records
/// each branch needs, and the forge's head of a review pull request.
pub struct Records<'a> {
    /// What branch `name` records it needs, or why that could not be read.
    pub requirements: &'a dyn Fn(&str) -> Result<Vec<crate::ids::Requirement>, String>,
    /// The head commit the forge reports for review pull request `number`
    /// on `repository`, or why it could not be asked.
    pub pull_head: &'a dyn Fn(&str, u64) -> Result<String, String>,
}

impl std::fmt::Debug for Records<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Records").finish_non_exhaustive()
    }
}

/// Every spelling of `fork`'s own repository, lowercase: what its
/// checkout's `origin` remote names, and the registry's `origin`, as
/// `knives depends` records a member branch.
pub fn own_repository_spellings(fork: &Fork<'_>) -> Vec<String> {
    let mut own: Vec<String> = [
        fork.checkout.remotes.get("origin").map(String::as_str),
        Some(fork.entry.origin.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter_map(crate::remote_url::remote_slug)
    .map(str::to_ascii_lowercase)
    .collect();
    own.dedup();
    own
}

/// Where a member's own change starts, for a release target's replay.
///
/// From the trunk, a member stacked on another branch (a pull request into
/// that branch) brings that branch's whole line with it: lines its siblings
/// rewrote and the release combined by hand read as dropped. So a stacked
/// member is replayed from the branch it is stacked on, which its record
/// names (`knives depends <member> --on <branch>`), never from the trunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberBase {
    /// It is seen stacked on no maintained branch and records no base the
    /// graph bears out: its change starts where it forked from the trunk.
    Trunk,
    /// Its record names the one branch or review pull request on the fork's
    /// own repository it needs, and the graph bears it out: its own change
    /// starts at `from`.
    Recorded { requirement: String, from: CommitId },
    /// Where its own change starts cannot be read: why. Only a replay from
    /// the trunk that reads empty or clean answers it; one that conflicts
    /// cannot be judged.
    Unknown(String),
}

/// What [`MemberBase::read`] reads a member's base from.
pub struct BaseContext<'a> {
    /// The upstream trunk.
    pub trunk: &'a CommitId,
    /// Every spelling of the fork's own repository, lowercase: its
    /// registry `origin` and its checkout's `origin` remote.
    pub own: &'a [String],
    /// The fork's maintained branches and their tips
    /// ([`release_model::carried_from_tips`]).
    pub branches: &'a [(BranchName, CommitId)],
    /// The head the forge reports for review pull request `number` on the
    /// fork's own repository `repository`, or why it could not be asked.
    pub pull_head: &'a dyn Fn(&str, u64) -> Result<String, String>,
}

impl std::fmt::Debug for BaseContext<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BaseContext")
            .field("trunk", &self.trunk)
            .field("own", &self.own)
            .field("branches", &self.branches)
            .finish_non_exhaustive()
    }
}

impl MemberBase {
    /// The base of the member at `tip`, named `branch` when a branch names
    /// it, from what its record says it needs (`recorded`, or why the record
    /// could not be read).
    ///
    /// It is stacked when the tip of another maintained branch is on its
    /// first-parent line since the trunk: the nearest such tip is where it
    /// left the branch it was built on. A branch merged into its line (a
    /// review branch that landed into it by a merge) is reached only through
    /// a merge's other parent, and stacks nothing.
    ///
    /// A record is used only where the graph bears it out, so that a wrong
    /// one cannot move the member's own commits into its base:
    /// - the record's head reaches the member (it landed into its base): its
    ///   own change starts where it left the base, read from the merge that
    ///   landed it ([`Repo::member_fork_point`]); a base that took it with no
    ///   merge leaves it unknown;
    /// - it is stacked: the record's head reaches the branch tip it was
    ///   built on, and its fork point with the member is that tip;
    /// - it is not stacked: the record's fork point with it is the trunk's,
    ///   so it starts from the trunk.
    ///
    /// Otherwise, and for a head on another repository, from a forge that
    /// does not answer or not in this checkout, it is unknown. Recording
    /// none or several, it is unknown when stacked and starts from the trunk
    /// otherwise.
    pub fn read(
        repo: &Repo,
        (branch, tip): (Option<&str>, &CommitId),
        recorded: Result<&[crate::ids::Requirement], &str>,
        context: &BaseContext<'_>,
    ) -> anyhow::Result<Self> {
        use crate::ids::Requirement;
        let line = repo.first_parent_line(tip, context.trunk)?;
        // The nearest branch tip below the member is the one it was built on.
        let stacked_on = line.iter().find_map(|commit| {
            context
                .branches
                .iter()
                .find(|(other, other_tip)| other_tip == commit && Some(other.as_str()) != branch)
        });
        let stacked_name = stacked_on.map(|(other, _)| other);
        let name = branch.unwrap_or("this revision");
        let record = |on: &str| {
            branch.map_or_else(
                || {
                    "`knives depends <branch> --on <branch it is stacked on>` on its branch, if \
                     it was built on that branch's commits"
                        .to_owned()
                },
                |branch| {
                    format!(
                        "`knives depends {branch} --on {on}` if {branch} was built on {on}'s \
                         commits, not on a copy of its own"
                    )
                },
            )
        };
        let on_fork: Vec<&Requirement> = match recorded {
            Ok(recorded) => recorded
                .iter()
                .filter(|requirement| !matches!(requirement, Requirement::Upstream { .. }))
                .collect(),
            Err(reason) => {
                return Ok(stacked_name.map_or(Self::Trunk, |stacked_on| {
                    Self::Unknown(format!(
                        "{name} is stacked on {stacked_on}, and what it records it needs could \
                         not be read: {reason}"
                    ))
                }));
            }
        };
        let [requirement] = on_fork.as_slice() else {
            let Some(stacked_on) = stacked_name else {
                return Ok(Self::Trunk);
            };
            return Ok(Self::Unknown(if on_fork.is_empty() {
                format!(
                    "{name} is stacked on {stacked_on}, and no record says which branch its own \
                     change starts from: record it with {}",
                    record(stacked_on.as_str())
                )
            } else {
                format!(
                    "{name} is stacked on {stacked_on} and records {} branches or review pull \
                     requests it needs ({}), so which one its own change starts from cannot be \
                     told",
                    on_fork.len(),
                    on_fork
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }));
        };
        let head = match Self::recorded_head(repo, requirement, context) {
            Ok(head) => head,
            Err(why) => {
                return Ok(Self::Unknown(format!(
                    "{name} records {requirement}, {why}"
                )));
            }
        };
        Ok(
            match Self::borne_out(repo, (tip, &head), stacked_on, context.trunk)? {
                Ok(Some(from)) => Self::Recorded {
                    requirement: requirement.to_string(),
                    from,
                },
                Ok(None) => Self::Trunk,
                Err(why) => Self::Unknown(format!("{name} records {requirement}, {why}")),
            },
        )
    }

    /// Where the own change of the member at `tip` starts, by its recorded
    /// base's `head`, where the graph bears the record out ([`Self::read`]):
    /// `Some` where it left the base, `None` for the trunk's fork point, or
    /// why the record cannot be used.
    fn borne_out(
        repo: &Repo,
        (tip, head): (&CommitId, &CommitId),
        stacked_on: Option<&(BranchName, CommitId)>,
        trunk: &CommitId,
    ) -> anyhow::Result<Result<Option<CommitId>, String>> {
        if repo.is_ancestor(tip, head)? {
            return Ok(repo.member_fork_point(tip, head)?.map_or_else(
                || {
                    Err(
                        "which took it with no merge, so its own change cannot be told from its \
                         base's"
                            .to_owned(),
                    )
                },
                |from| Ok(Some(from)),
            ));
        }
        let from = repo.fork_point(tip, head)?;
        Ok(match stacked_on {
            Some((on, on_tip)) => {
                if repo.is_ancestor(on_tip, head)? && repo.is_ancestor(&from, on_tip)? {
                    Ok(Some(from))
                } else {
                    Err(format!(
                        "but it was built on {on}, and its fork point with the record is not \
                         {on}'s tip"
                    ))
                }
            }
            None => {
                if repo.is_ancestor(&from, trunk)? {
                    Ok(None)
                } else {
                    Err(
                        "but no maintained branch's tip is on its line, so its fork point with \
                         the record is one of its own commits"
                            .to_owned(),
                    )
                }
            }
        })
    }

    /// The head of the one branch or review pull request a member records
    /// it needs, or why it cannot be read here.
    fn recorded_head(
        repo: &Repo,
        requirement: &crate::ids::Requirement,
        context: &BaseContext<'_>,
    ) -> Result<CommitId, String> {
        use crate::ids::Requirement;
        let own = |repository: &str| {
            context
                .own
                .iter()
                .any(|own| own.eq_ignore_ascii_case(repository))
        };
        match requirement {
            Requirement::ForkBranch { repository, branch } if own(repository) => context
                .branches
                .iter()
                .find(|(name, _)| name == branch)
                .map(|(_, tip)| tip.clone())
                .map_or_else(|| repo.resolve_commit(&format!("{branch}@origin")), Ok)
                .map_err(|_| "which this checkout does not hold, here or on origin".to_owned()),
            Requirement::ForkPull { repository, number } if own(repository) => {
                let oid = (context.pull_head)(repository, *number)
                    .map_err(|reason| format!("whose head the forge did not report: {reason}"))?;
                repo.resolve_commit(&oid).map_err(|_| {
                    format!(
                        "whose head {} is not in this checkout: fetch origin",
                        crate::ids::short_id(&oid)
                    )
                })
            }
            _ => Err("which is not on this fork's own repository".to_owned()),
        }
    }
}

#[derive(Debug)]
pub struct CheckInput<'a> {
    pub repo: &'a Repo,
    pub revision: &'a str,
    pub tip: &'a CommitId,
    /// The upstream trunk: a release target replays the tip's change from
    /// its fork point with it, unless it is stacked (`base`).
    pub trunk: &'a CommitId,
    /// Where the tip's own change starts ([`MemberBase`]).
    pub base: MemberBase,
    /// The tip's fork point with the trunk, read once, by the first release
    /// target that needs it.
    fork_point: std::cell::OnceCell<CommitId>,
}

impl<'a> CheckInput<'a> {
    pub fn new(
        repo: &'a Repo,
        revision: &'a str,
        (tip, base): (&'a CommitId, MemberBase),
        trunk: &'a CommitId,
    ) -> Self {
        Self {
            repo,
            revision,
            tip,
            trunk,
            base,
            fork_point: std::cell::OnceCell::new(),
        }
    }

    /// The tip's fork point with the trunk ([`Repo::fork_point`]).
    fn fork_point(&self) -> Result<&CommitId, crate::jj::JjError> {
        if let Some(fork_point) = self.fork_point.get() {
            return Ok(fork_point);
        }
        let fork_point = self.repo.fork_point(self.tip, self.trunk)?;
        Ok(self.fork_point.get_or_init(|| fork_point))
    }
}

/// The three-way verdict of one revision against one target.
///
/// An ancestor tip is carried-exact against the upstream trunk, which knives
/// does not compose. Against a release it is carried-exact only when the
/// release holds every line of its own change as the tip left it: a release
/// merge names its members as parents, so every member is an ancestor
/// whatever the merge's tree holds, and a hand resolution can drop a
/// member's change while keeping the member a parent. Its own change starts
/// where it forked from the trunk, or, for a member whose record the graph
/// bears out, where it left its base ([`MemberBase`]): from the trunk a
/// stacked member would bring its base's line, which the release can hold
/// combined with the work of others by hand. Its own change replaying empty
/// onto the release shows it held. A replay that leaves a clean diff shows
/// part of it gone, reverted or dropped: not carried. A replay that
/// conflicts is read file by file and hunk by hunk
/// ([`Repo::held_replay_outcome`]): carried-exact when every line the tip
/// wrote is there as it left it, whatever changed beside it; not carried
/// when the rest holds and a hunk is missing; conflicted otherwise, as when
/// later work or a resolution rewrote a line of it. A member whose base
/// cannot be read is answered from the trunk only when that reading is
/// empty or clean; one that conflicts is an error naming why.
pub fn check(input: &CheckInput<'_>, target: &Target) -> anyhow::Result<CarryCheck> {
    if input.repo.is_ancestor(input.tip, &target.commit)? {
        let verdict = match target.role {
            TargetRole::UpstreamTrunk => CarryVerdict::CarriedExact,
            TargetRole::LiveRelease | TargetRole::SupersededRelease => {
                let from = match &input.base {
                    MemberBase::Trunk | MemberBase::Unknown(_) => input.fork_point()?.clone(),
                    MemberBase::Recorded { from, .. } => from.clone(),
                };
                let outcome =
                    match input
                        .repo
                        .fork_point_replay_outcome(input.tip, &from, &target.commit)?
                    {
                        RebaseOutcome::Conflicted => {
                            input
                                .repo
                                .held_replay_outcome(input.tip, &from, &target.commit)?
                        }
                        outcome => outcome,
                    };
                match (outcome, &input.base) {
                    (RebaseOutcome::Conflicted, MemberBase::Unknown(reason)) => anyhow::bail!(
                        "its change since the trunk conflicts with the release, and {reason}"
                    ),
                    (RebaseOutcome::Empty, _) => CarryVerdict::CarriedExact,
                    (RebaseOutcome::CleanNonEmpty, _) => CarryVerdict::NotCarried,
                    (RebaseOutcome::Conflicted, _) => CarryVerdict::Conflicted,
                }
            }
        };
        let evidence = if verdict == CarryVerdict::CarriedExact {
            input.tip.clone()
        } else {
            target.commit.clone()
        };
        return Ok(CarryCheck { verdict, evidence });
    }

    let verdict = match input.repo.tree_replay_outcome(input.tip, &target.commit)? {
        RebaseOutcome::Empty => CarryVerdict::CarriedRewritten,
        RebaseOutcome::CleanNonEmpty => CarryVerdict::NotCarried,
        RebaseOutcome::Conflicted => CarryVerdict::Conflicted,
    };
    if verdict != CarryVerdict::CarriedRewritten {
        for rewrite in input.repo.standing_rewrites_of(input.tip)? {
            if input.repo.is_ancestor(&rewrite, &target.commit)? {
                return Ok(CarryCheck {
                    verdict: CarryVerdict::CarriedRebased,
                    evidence: rewrite,
                });
            }
        }
    }
    Ok(CarryCheck {
        verdict,
        evidence: target.commit.clone(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use crate::config::RepoEntry;
    use crate::detect::BookmarkTips;
    use crate::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RemoteName};

    use super::{
        BranchCarriage, CarriesReport, CarryCheck, CarryVerdict, CensusInput, CensusReport,
        CensusTargets, Target, TargetCheck, TargetRole, census_member, orphan_status,
        render_carries, render_census, targets,
    };

    fn local(name: &str) -> BookmarkRef {
        BookmarkRef::Local(BranchName::new(name))
    }

    fn remote(name: &str, remote: &str) -> BookmarkRef {
        BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(remote),
        }
    }

    fn tips(entries: Vec<(BookmarkRef, &str)>) -> BookmarkTips {
        entries
            .into_iter()
            .map(|(reference, commit)| (reference, CommitId::new(commit)))
            .collect()
    }

    #[test]
    fn orphan_status_requires_complete_carriage_and_answered_pull_state() {
        let not_carried = [CarryVerdict::NotCarried];

        let primary_incomplete = orphan_status(false, true, not_carried.into_iter(), None);
        assert_eq!(primary_incomplete.status, None);
        assert!(!primary_incomplete.list_as_orphan);

        let escalation_incomplete = orphan_status(true, false, not_carried.into_iter(), None);
        assert_eq!(escalation_incomplete.status, None);
        assert!(!escalation_incomplete.list_as_orphan);

        let pull_unknown = orphan_status(true, true, not_carried.into_iter(), None);
        assert_eq!(pull_unknown.status, None);
        assert!(
            pull_unknown.list_as_orphan,
            "the qualified unknown-pull listing remains actionable"
        );

        let proven = orphan_status(true, true, not_carried.into_iter(), Some(false));
        assert_eq!(proven.status, Some(true));
        assert!(proven.list_as_orphan);

        let blocked = orphan_status(true, true, not_carried.into_iter(), Some(true));
        assert_eq!(blocked.status, Some(false));
        assert!(!blocked.list_as_orphan);
    }

    #[test]
    fn one_release_name_at_two_commits_is_two_live_targets() {
        let release = "release/2026-08-30.1";
        let local_ref = local(release);
        let origin_ref = remote(release, "origin");
        let targets = targets(
            &tips(vec![(local_ref.clone(), "a"), (origin_ref.clone(), "b")]),
            &ReleaseScheme::Dated,
            ("trunk", CommitId::new("trunk")),
            "origin",
        );

        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[0].commit, CommitId::new("a"));
        assert_eq!(targets[0].refs, vec![local_ref]);
        assert_eq!(targets[1].role, TargetRole::LiveRelease);
        assert_eq!(targets[1].commit, CommitId::new("b"));
        assert_eq!(targets[1].refs, vec![origin_ref]);
        assert_eq!(targets[2].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn split_release_targets_exclude_a_stale_origin_release_ref() {
        let name = "release/2026-08-30";
        let release_ref = remote(name, "release");
        let targets = targets(
            &tips(vec![
                (remote(name, "origin"), "stale-origin"),
                (release_ref.clone(), "published"),
            ]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
            "release",
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[0].commit, CommitId::new("published"));
        assert_eq!(targets[0].refs, vec![release_ref]);
        assert_eq!(targets[1].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn an_equal_release_url_keeps_origin_release_as_a_live_target() {
        let entry = RepoEntry {
            release: Some("https://forge.invalid/ours/demo.git".to_owned()),
            ..RepoEntry::new(
                "https://forge.invalid/up/demo.git",
                "https://forge.invalid/ours/demo.git",
            )
        };
        let origin_ref = remote("release/2026-08-30", "origin");

        let targets = targets(
            &tips(vec![(origin_ref.clone(), "published")]),
            &entry.release_scheme(),
            ("main", CommitId::new("trunk")),
            entry.publish_remote(),
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[0].refs, vec![origin_ref]);
    }

    #[test]
    fn superseded_names_and_the_trunk_are_their_own_roles() {
        let old_local = local("release/2026-08-29");
        let old_origin = remote("release/2026-08-29", "origin");
        let trunk_ref = remote("main", "upstream");
        let targets = targets(
            &tips(vec![
                (old_local.clone(), "old"),
                (old_origin.clone(), "old"),
                (local("release/2026-08-30"), "live"),
                (trunk_ref.clone(), "trunk"),
            ]),
            &ReleaseScheme::Dated,
            ("main@upstream", CommitId::new("trunk")),
            "origin",
        );

        assert_eq!(
            targets.iter().map(|target| target.role).collect::<Vec<_>>(),
            vec![
                TargetRole::LiveRelease,
                TargetRole::UpstreamTrunk,
                TargetRole::SupersededRelease,
            ]
        );
        assert_eq!(targets[0].commit, CommitId::new("live"));
        assert_eq!(targets[1].commit, CommitId::new("trunk"));
        assert_eq!(targets[1].refs, vec![trunk_ref]);
        assert_eq!(targets[2].commit, CommitId::new("old"));
        assert_eq!(targets[2].refs, vec![old_local, old_origin]);
    }

    #[test]
    fn upstream_release_refs_are_not_ours_and_not_targets() {
        let targets = targets(
            &tips(vec![(remote("release/2026-08-30", "upstream"), "upstream")]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
            "origin",
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].role, TargetRole::UpstreamTrunk);
        assert_eq!(targets[0].commit, CommitId::new("trunk"));
        assert_eq!(targets[0].refs, Vec::<BookmarkRef>::new());
    }

    #[test]
    fn fixed_release_refs_are_all_live_targets() {
        let targets = targets(
            &tips(vec![
                (local("integration"), "local"),
                (remote("integration", "release"), "release"),
            ]),
            &ReleaseScheme::Fixed(BranchName::new("integration")),
            ("main", CommitId::new("trunk")),
            "release",
        );

        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[1].role, TargetRole::LiveRelease);
        assert_eq!(targets[2].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn a_commit_named_by_live_and_superseded_releases_is_live() {
        let targets = targets(
            &tips(vec![
                (local("release/2026-08-29"), "shared"),
                (local("release/2026-08-30"), "shared"),
            ]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
            "origin",
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].role, TargetRole::LiveRelease);
        assert_eq!(targets[0].commit, CommitId::new("shared"));
        assert_eq!(targets[1].role, TargetRole::UpstreamTrunk);
    }

    #[test]
    fn unparseable_dated_release_prefixes_are_not_live() {
        let targets = targets(
            &tips(vec![(local("release/not-a-date"), "invalid")]),
            &ReleaseScheme::Dated,
            ("main", CommitId::new("trunk")),
            "origin",
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].role, TargetRole::UpstreamTrunk);
        assert_eq!(targets[1].role, TargetRole::SupersededRelease);
    }

    #[test]
    fn incomplete_primary_matrix_never_checks_superseded_targets() {
        // A regression would call the superseded target after a no-common-ancestor
        // primary error. The call log is the observable check budget.
        let target = |role, commit| Target {
            refs: Vec::new(),
            commit: CommitId::new(commit),
            role,
        };
        let primary = [
            target(TargetRole::LiveRelease, "live"),
            target(TargetRole::UpstreamTrunk, "trunk"),
        ];
        let superseded = [target(TargetRole::SupersededRelease, "historical")];
        let mut problems = Vec::new();
        let mut calls = Vec::new();

        let input = CensusInput {
            branch: "feat/unanswered".to_owned(),
            tip: CommitId::new("tip"),
            base: super::MemberBase::Trunk,
            in_open_pull: None,
        };
        let trunk = CommitId::new("trunk");
        let targets = CensusTargets {
            primary: &primary,
            superseded: &superseded,
            fallback: "main",
            trunk: &trunk,
        };
        let member = census_member(&input, targets, &mut problems, |target| {
            calls.push(target.role);
            match target.role {
                TargetRole::LiveRelease => Ok(CarryCheck {
                    verdict: CarryVerdict::NotCarried,
                    evidence: target.commit.clone(),
                }),
                TargetRole::UpstreamTrunk => {
                    Err(anyhow::anyhow!("no unique common ancestor with trunk"))
                }
                TargetRole::SupersededRelease => Err(anyhow::anyhow!("must not escalate")),
            }
        });

        assert_eq!(
            calls,
            [TargetRole::LiveRelease, TargetRole::UpstreamTrunk],
            "an unanswered primary matrix must not spend a superseded check"
        );
        assert_eq!(member.carriage.checks.len(), 1);
        assert_eq!(member.carriage.orphan, None);
        assert_eq!(problems.len(), 1);
    }

    #[test]
    fn a_census_and_a_carries_report_escape_their_notes_and_problems() {
        // Both quote jj and the forge: "pull request state unavailable:"
        // carries whatever the forge said.
        let census = CensusReport {
            repo: "demo".to_owned(),
            rows: Vec::new(),
            orphans: Vec::new(),
            notes: vec![
                "pull request state unavailable: \u{1b}[2J".to_owned(),
                "and \u{7}then".to_owned(),
            ],
            problems: vec!["jj said\nError: \u{1b}[31mno".to_owned()],
        };
        let carries = CarriesReport {
            repo: "demo".to_owned(),
            revision: "@".to_owned(),
            checks: Vec::new(),
            notes: vec!["gh said \u{9b}2J".to_owned()],
            problems: vec!["could not read \u{1b}]0;pwned\u{7}".to_owned()],
        };

        let census = render_census(&census);
        let carries = render_carries(&carries);

        for text in [&census, &carries] {
            assert!(!text.contains('\u{1b}'), "was: {text:?}");
            assert!(!text.contains('\u{7}'), "was: {text:?}");
            assert!(!text.contains('\u{9b}'), "was: {text:?}");
        }
        assert!(
            census.contains("pull request state unavailable: \u{fffd}[2J"),
            "was: {census:?}"
        );
        assert!(
            census.contains("unanswered: jj said; Error: \u{fffd}[31mno"),
            "was: {census:?}"
        );
        assert!(
            carries.contains("  note: gh said \u{fffd}2J"),
            "was: {carries:?}"
        );
    }

    #[test]
    fn a_census_escapes_the_branch_orphan_and_target_names_it_read_from_refs() {
        // git takes `refs/heads/feat<U+009B>m`, and jj imports it from a remote
        // as the bookmark `feat<U+009B>m@origin`.
        let check = || TargetCheck {
            target: "release/\u{9b}2J".to_owned(),
            commit: CommitId::new("aaaa"),
            role: TargetRole::LiveRelease,
            verdict: CarryVerdict::CarriedExact,
            evidence: CommitId::new("bbbb"),
        };
        let census = CensusReport {
            repo: "demo".to_owned(),
            rows: vec![BranchCarriage {
                branch: "feat\u{9b}m".to_owned(),
                tip: CommitId::new("cccc"),
                checks: vec![check()],
                in_open_pull: Some(false),
                orphan: Some(true),
            }],
            orphans: vec!["feat\u{9b}m".to_owned()],
            notes: Vec::new(),
            problems: Vec::new(),
        };
        let carries = CarriesReport {
            repo: "demo".to_owned(),
            revision: "@".to_owned(),
            checks: vec![check()],
            notes: Vec::new(),
            problems: Vec::new(),
        };

        let census = render_census(&census);
        let carries = render_carries(&carries);

        for text in [&census, &carries] {
            assert!(
                !text.chars().any(|c| c.is_control() && c != '\n'),
                "was: {text:?}"
            );
        }
        assert!(census.contains("  feat\u{fffd}m @ cccc"), "was: {census:?}");
        assert!(census.contains("\n  feat\u{fffd}m"), "was: {census:?}");
        assert!(
            carries.contains("release/\u{fffd}2J @ aaaa"),
            "was: {carries:?}"
        );
    }
}
