//! Whether content is actually carried — by replay and ancestry, never text.
//!
//! The audit's worst near-miss class was a branch deleted while its content
//! was uncarried. The verdicts here are content-based only: sha ancestry for
//! carried-exact, a three-way tree merge for carried-rewritten (jj divergent
//! change-ids force tree comparison — the same change id can name two
//! different trees), and merge conflicts for human judgment. A net-zero
//! revision is vacuously carried: it contributes no content for the target to
//! lack. Every verdict names an evidence commit a notch can cite and a later
//! reader can re-resolve.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, RebaseOutcome};
use crate::forge::{Forge, index_pulls};
use crate::ids::{
    BookmarkRef, BranchName, CommitId, ReleaseScheme, RepoName, is_our_release,
    strict_dated_release,
};
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
    /// The revision's tip is an ancestor of the target: carried as-is.
    CarriedExact,
    /// Its net tree change leaves the target unchanged, whether the same content
    /// arrived through different commits or the revision itself has no net content.
    CarriedRewritten,
    NotCarried,
    /// The replay conflicted while the target itself is clean: some content
    /// is there or unrelated work touched the same files; judge by eye.
    Conflicted,
}

impl CarryVerdict {
    pub const fn carried(self) -> bool {
        matches!(self, Self::CarriedExact | Self::CarriedRewritten)
    }
}

/// One verdict with the commit that proves it: the revision tip for
/// carried-exact (it IS in the target's ancestry), the target commit
/// otherwise (the tree the replay was judged against).
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

/// The complete answer to a `release carries` query.
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
    repo_name: &RepoName,
    entry: &RepoEntry,
    forge: Option<&dyn Forge>,
    options: CensusOptions<'_>,
) -> anyhow::Result<CensusReport> {
    let repo = Repo::open(&entry.path)?;
    let trunk_name = entry.upstream_trunk();
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
        (trunk_name.as_str(), trunk),
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
    } = census_pulls(forge, entry, branches.as_slice(), options.cache_root);
    notes.extend(pull_notes);

    let members: Vec<CensusInput> = branches
        .into_iter()
        .map(|(branch, tip)| CensusInput {
            in_open_pull: pull_requests_checked
                .then(|| branch_pulls.get(&branch).copied().unwrap_or(false)),
            branch: branch.to_string(),
            tip,
        })
        .collect();
    let targets = CensusTargets {
        primary: &primary,
        superseded: &superseded,
        fallback: trunk_name.as_str(),
    };
    let (members, problems) = census_rows(&entry.path, &members, targets, options.workers);
    let orphans = members
        .iter()
        .filter(|member| member.list_as_orphan)
        .map(|member| member.carriage.branch.clone())
        .collect();
    let rows = members.into_iter().map(|member| member.carriage).collect();
    Ok(CensusReport {
        repo: repo_name.to_string(),
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
    entry: &RepoEntry,
    branches: &[(BranchName, CommitId)],
    cache_root: Option<&Path>,
) -> CensusPulls {
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
        path: &entry.path,
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
                                census_member(input, targets, &mut problems, |target| {
                                    let check_input = CheckInput {
                                        repo: &repo,
                                        revision: &input.branch,
                                        tip: &input.tip,
                                    };
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
pub fn render_census(report: &CensusReport) -> String {
    let mut lines = vec![format!("{}: census", report.repo)];
    let mut notes = report.notes.iter();
    if let Some(primary) = notes.next() {
        lines.push(primary.clone());
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
        lines.extend(report.orphans.iter().map(|orphan| format!("  {orphan}")));
    }
    lines.extend(notes.map(|note| format!("note: {note}")));
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("unanswered: {problem}")),
    );
    lines.join("\n")
}

fn render_census_row(lines: &mut Vec<String>, row: &BranchCarriage) {
    lines.push(format!("  {} @ {}", row.branch, short(&row.tip)));
    lines.extend(row.checks.iter().map(|check| render_check(check, "    ")));
    let pull = match row.in_open_pull {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    lines.push(format!("    open pull request: {pull}"));
}

/// Render the human-readable form of a multi-target carriage report.
pub fn render_carries(report: &CarriesReport) -> String {
    let mut lines = vec![format!("{}: {}", report.repo, report.revision)];
    lines.extend(report.checks.iter().map(|check| render_check(check, "  ")));
    lines.extend(report.notes.iter().map(|note| format!("  note: {note}")));
    lines.extend(
        report
            .problems
            .iter()
            .map(|problem| format!("  unanswered: {problem}")),
    );
    lines.join("\n")
}

fn render_check(check: &TargetCheck, indent: &str) -> String {
    let verdict = match check.verdict {
        CarryVerdict::CarriedExact => "carried-exact",
        CarryVerdict::CarriedRewritten => "carried-rewritten",
        CarryVerdict::NotCarried => "NOT carried",
        CarryVerdict::Conflicted => "conflicted",
    };
    let reason = match check.verdict {
        CarryVerdict::CarriedExact => "tip is an ancestor",
        CarryVerdict::CarriedRewritten => "no net content remains",
        CarryVerdict::NotCarried => "replay leaves real diffs",
        CarryVerdict::Conflicted => "judge by eye",
    };
    let target = if check.target.is_empty() {
        match check.role {
            TargetRole::UpstreamTrunk => "upstream trunk",
            TargetRole::LiveRelease | TargetRole::SupersededRelease => "unnamed release",
        }
    } else {
        check.target.as_str()
    };
    format!(
        "{indent}{verdict:<19}{target} @ {}  (evidence {}: {reason})",
        short(&check.commit),
        short(&check.evidence),
    )
}

fn short(commit: &CommitId) -> String {
    commit.as_str().chars().take(12).collect()
}

/// Every check target for this repository: each distinct commit named by our
/// release refs (grouped, so one name at two commits is two targets), plus the
/// upstream trunk.
///
/// Live/superseded comes from `strict_dated_release` ordering; under a fixed
/// scheme every release ref of the configured name is Live.
pub fn targets(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    trunk: (&str, CommitId),
    publish_remote: &str,
) -> Vec<Target> {
    let newest_release = match scheme {
        ReleaseScheme::Dated => tips
            .keys()
            .filter(|reference| is_our_release(reference, scheme, publish_remote))
            .filter_map(|reference| strict_dated_release(reference.branch().as_str()))
            .max(),
        ReleaseScheme::Fixed(_) => None,
    };
    let mut grouped =
        BTreeMap::<CommitId, (Vec<BookmarkRef>, TargetRole, Option<(String, u32)>)>::new();

    for (reference, commit) in tips
        .iter()
        .filter(|(reference, _)| is_our_release(reference, scheme, publish_remote))
    {
        let dated_name = strict_dated_release(reference.branch().as_str());
        let role = match scheme {
            ReleaseScheme::Dated
                if matches!(
                    (dated_name.as_ref(), newest_release.as_ref()),
                    (Some(dated_name), Some(newest_release)) if dated_name == newest_release
                ) =>
            {
                TargetRole::LiveRelease
            }
            ReleaseScheme::Dated => TargetRole::SupersededRelease,
            ReleaseScheme::Fixed(_) => TargetRole::LiveRelease,
        };
        let entry = grouped.entry(commit.clone()).or_insert_with(|| {
            (
                Vec::new(),
                TargetRole::SupersededRelease,
                dated_name.clone(),
            )
        });
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

#[derive(Debug)]
pub struct CheckInput<'a> {
    pub repo: &'a Repo,
    pub revision: &'a str,
    pub tip: &'a CommitId,
}

/// The three-way verdict of one revision against one target.
pub fn check(input: &CheckInput<'_>, target: &Target) -> anyhow::Result<CarryCheck> {
    if input.repo.is_ancestor(input.tip, &target.commit)? {
        return Ok(CarryCheck {
            verdict: CarryVerdict::CarriedExact,
            evidence: input.tip.clone(),
        });
    }

    let verdict = match input.repo.tree_replay_outcome(input.tip, &target.commit)? {
        RebaseOutcome::Empty => CarryVerdict::CarriedRewritten,
        RebaseOutcome::CleanNonEmpty => CarryVerdict::NotCarried,
        RebaseOutcome::Conflicted => CarryVerdict::Conflicted,
    };
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
    use std::path::PathBuf;

    use super::{
        CarryCheck, CarryVerdict, CensusInput, CensusTargets, Target, TargetRole, census_member,
        orphan_status, targets,
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
            path: PathBuf::from("/tmp/demo"),
            upstream: "https://forge.invalid/up/demo.git".to_owned(),
            origin: "https://forge.invalid/ours/demo.git".to_owned(),
            base: None,
            release: Some("https://forge.invalid/ours/demo.git".to_owned()),
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
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
        assert!(targets[0].refs.is_empty());
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
            in_open_pull: None,
        };
        let targets = CensusTargets {
            primary: &primary,
            superseded: &superseded,
            fallback: "main",
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
}
