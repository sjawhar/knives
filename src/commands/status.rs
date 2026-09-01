//! `knives status`: per-branch state and all four detectors against a live repo.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::cli::Exit;
use crate::config::{Registry, RepoEntry, Role};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, LandedVerdict, Subject, classify_landed, divergent_changes,
    double_checkout, stale_parents,
};
use crate::forge::{
    ChecksSummary, Forge, PullDetails, PullIndex, PullRequest, PullSummary, index_pulls,
};
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, ReleaseScheme, RepoName, is_release_name,
    pull_number_from_bookmark,
};
use crate::jj::{JjError, Repo, branches_past, probe_landed};
use crate::ledger::{Entry as Notch, Ledger};
use crate::release_model::{double_cut_findings, release_order};
use crate::store::Store;

use crate::ids::is_our_release;

pub mod phases;
pub mod render;
mod rows;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BranchRow {
    pub name: BranchName,
    pub state: BranchState,
    /// Short (12-char) commit id; absent when the bookmark is divergent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
    /// Present only when the local branch does not cleanly match origin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push: Option<PushRelation>,
    /// Origin's differing short tip. Its relation lives in `push`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_tip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullCell>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landed: Option<LandedVerdict>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimCell>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen: Option<SeenWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notch: Option<LastNotch>,
}

impl BranchRow {
    fn bare(name: BranchName) -> Self {
        Self {
            name,
            state: BranchState::Unknown,
            tip: None,
            push: None,
            origin_tip: None,
            pr: None,
            review: None,
            checks: None,
            landed: None,
            flags: Vec::new(),
            claim: None,
            last_seen: None,
            seen: None,
            workspace: None,
            notch: None,
        }
    }
}

/// A shadowed pull request, compact enough for a row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PriorPull {
    pub number: u64,
    pub state: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PullCell {
    pub number: u64,
    pub state: String,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub draft: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stated: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prior: Vec<PriorPull>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClaimCell {
    pub id: String,
    pub kind: crate::store::OwnerKind,
    pub since: String,
    pub why: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BranchState {
    ForkOnly,
    Divergent,
    Landed,
    Conflicted,
    ChecksFailing,
    ChangesRequested,
    Approved,
    Draft,
    AwaitingReview,
    Merged,
    Closed,
    NoPr,
    Unknown,
}

impl fmt::Display for BranchState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ForkOnly => "fork-only",
            Self::Divergent => "divergent",
            Self::Landed => "landed",
            Self::Conflicted => "conflicted",
            Self::ChecksFailing => "checks-failing",
            Self::ChangesRequested => "changes-requested",
            Self::Approved => "approved",
            Self::Draft => "draft",
            Self::AwaitingReview => "awaiting-review",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::NoPr => "no-pr",
            Self::Unknown => "unknown",
        })
    }
}

/// Internal ancestry facts, converted to the report's wider `PushRelation` taxonomy in rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginRelation {
    Ahead,
    Behind,
    Diverged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PushRelation {
    Unpushed,
    UnpushedCommits,
    Behind,
    Diverged,
    Unresolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeenWindow {
    NoneSinceClaim,
    NoneWithinWindow,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct ForgeStatus {
    pub consulted: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, serde::Serialize)]
pub struct FindingGroup {
    pub kind: FindingKind,
    pub count: usize,
    pub subjects: Vec<String>,
}

/// The most relevant ledger entry for one status row and the entries it masks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastNotch {
    pub ts: String,
    pub kind: crate::ledger::Kind,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    pub count: usize,
}

fn collapsed_notch_text(text: &str) -> String {
    const MAX_TEXT: usize = 120;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut capped: String = text.chars().take(MAX_TEXT).collect();
    if text.chars().count() > MAX_TEXT {
        capped.push('…');
    }
    capped
}

impl LastNotch {
    fn of<'a>(entries: impl Iterator<Item = &'a Notch>) -> Option<Self> {
        let mut count = 0;
        let mut newest = None;
        let mut newest_note = None;
        for entry in entries {
            count += 1;
            newest = Some(entry);
            if entry.kind == crate::ledger::Kind::Note {
                newest_note = Some(entry);
            }
        }
        let entry = newest_note.or(newest)?;
        Some(Self {
            ts: entry.ts.clone(),
            kind: entry.kind,
            text: collapsed_notch_text(&entry.text),
            disposition: entry.disposition.clone(),
            count,
        })
    }
}

/// The repo-scoped portion of its ledger: facts about the repository rather
/// than a branch, which therefore have no branch-row cell to carry them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoNotches {
    pub count: usize,
    pub last: LastNotch,
}

/// Field order is serde order and text-presentation order.
#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub repo: String,
    pub trunk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_release: Option<String>,
    pub forge: ForgeStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    pub branches: Vec<BranchRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<FindingGroup>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub releases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_notches: Option<RepoNotches>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_workspaces: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub struct Options<'a> {
    pub probe: bool,
    pub forge: Option<&'a dyn Forge>,
    /// Resolved cache root. `None` disables cache reads and persistence, for library callers.
    pub cache: Option<&'a std::path::Path>,
    /// Needed because a branch's requirements may name other managed repos.
    pub registry: Option<&'a Registry>,
    /// This repository's ledger, for the per-branch breadcrumb. `None` reads none.
    pub ledger: Option<&'a Ledger>,
    /// How many threads the landed probes may use. `1` is serial.
    ///
    /// Set below the machine's parallelism when several repositories are gathered
    /// at once, so `--all` cannot multiply one repository's probe threads by the
    /// size of the registry.
    pub workers: usize,
}

/// Where a status run spent its time.
///
/// Not a report field: it measures this run rather than describing the
/// repository, and every number would change the JSON contract for readers who
/// did not ask. Printed to stderr when `KNIVES_TIMING` is set — an environment
/// variable rather than `--verbose`, because that flag already selects how
/// findings are grouped.
#[derive(Debug, Default, Clone, Copy)]
pub struct Timings {
    /// Opening the repository and reading its bookmark tips.
    pub repository: std::time::Duration,
    /// Opening the independent health handle and gathering stale-copy, workspace,
    /// divergent-change, and conflicted-bookmark findings.
    pub health: std::time::Duration,
    /// Detecting visible changes that exist as multiple commits.
    pub divergent_changes: std::time::Duration,
    /// Scanning releases for stale parents.
    pub releases: std::time::Duration,
    /// Preparing maintained branches, references, claims, and forge inputs.
    pub setup: std::time::Duration,
    /// Forge identity, discovery sweep or reseed, live facts batch, and every stated-pull and
    /// dependency lookup. Forge and probes overlap, so `total` is wall time rather than a sum.
    pub forge: std::time::Duration,
    /// Replaying branches onto the upstream trunk.
    pub probes: std::time::Duration,
    /// Comparing each local branch tip to its origin counterpart.
    pub origin_relations: std::time::Duration,
    /// Constructing rows for conflicted local bookmarks.
    pub divergent_rows: std::time::Duration,
    /// Finding branches carried by another branch.
    pub carried_findings: std::time::Duration,
    /// Comparing each maintained branch's changed paths for overlap.
    pub touching: std::time::Duration,
    /// Loading claims, workspaces, and claim overlap findings.
    pub claims: std::time::Duration,
    /// Final local finding folds and forge-cache persistence.
    pub report: std::time::Duration,
    pub total: std::time::Duration,
}

impl Timings {
    pub fn line(&self, repo: &str) -> String {
        format!(
            "timing {repo}: repository-open {}ms health {}ms divergent-changes {}ms releases {}ms setup {}ms forge {}ms probes {}ms origin-relations {}ms divergent-rows {}ms carried-findings {}ms touching {}ms claims {}ms report {}ms total {}ms",
            self.repository.as_millis(),
            self.health.as_millis(),
            self.divergent_changes.as_millis(),
            self.releases.as_millis(),
            self.setup.as_millis(),
            self.forge.as_millis(),
            self.probes.as_millis(),
            self.origin_relations.as_millis(),
            self.divergent_rows.as_millis(),
            self.carried_findings.as_millis(),
            self.touching.as_millis(),
            self.claims.as_millis(),
            self.report.as_millis(),
            self.total.as_millis()
        )
    }
}

/// Whether phase timings were asked for.
pub fn timing_enabled() -> bool {
    crate::timing::enabled()
}

impl fmt::Debug for Options<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn Forge` cannot derive Debug, and what matters for diagnosis is
        // whether a forge was consulted at all, not which one.
        f.debug_struct("Options")
            .field("probe", &self.probe)
            .field("forge", &self.forge.is_some())
            .finish()
    }
}

struct ReleaseScan<'a> {
    path: &'a std::path::Path,
    tips: &'a BookmarkTips,
    scheme: &'a ReleaseScheme,
    publish_remote: &'a str,
}

/// Which releases were scanned, what was found, and how many were skipped.
///
/// Extracted from `gather` because that function had grown past what one
/// reviewer can hold at once, not to be reused.
fn scan_releases(
    repo: &Repo,
    input: &ReleaseScan<'_>,
) -> anyhow::Result<(Vec<String>, Vec<Finding>, usize)> {
    let (releases, skipped) = releases_to_scan(input.tips, input.scheme, input.publish_remote);
    let mut names = Vec::new();
    let mut findings = Vec::new();
    for (release, commit) in &releases {
        names.push(release.to_string());
        // Resolve by commit id, never by the bookmark's display form. A remote
        // bookmark rendered `name@remote` is not reliably resolvable as a
        // revset, and the tip map already carries the commit.
        let mut stale = stale_parents(&repo.parents_of(commit.as_str())?, input.tips);
        // Say where the branch went, not just that nothing points at the parent.
        // `parents_of` only reports bookmarks pointing AT a parent, so the pure
        // detector can never produce the "feat/x is now <id>" payload.
        for finding in &mut stale {
            let Subject::Commit(parent) = finding.subject.clone() else {
                continue;
            };
            if let Ok(moved) = branches_past(input.path, &parent)
                && !moved.is_empty()
            {
                let where_now = moved
                    .iter()
                    .map(|(branch, tip)| format!("{branch} is now {}", short(tip.as_str())))
                    .collect::<Vec<_>>()
                    .join(", ");
                finding.detail = format!(
                    "parent {} is no longer the tip of its branch ({where_now})",
                    short(parent.as_str())
                );
            }
        }
        findings.extend(stale);
    }
    Ok((names, findings, skipped))
}

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
    fn record(
        &mut self,
        branch: &BranchName,
        requirement: &crate::ids::Requirement,
        state: Option<&str>,
    ) {
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
    let mut grouped: BTreeMap<RepoName, Vec<(BranchName, crate::ids::Requirement)>> =
        BTreeMap::new();
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
struct DependencyInput<'a, 'forge, 'snapshot> {
    report: &'a mut Report,
    findings: &'a mut Vec<Finding>,
    name: &'a RepoName,
    store: &'a Store,
    options: &'a Options<'forge>,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    timings: &'a mut Timings,
}

fn add_dependency_findings(input: DependencyInput<'_, '_, '_>) {
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

/// One sentence naming every branch that could not be judged.
///
/// One problem per branch meant ten copies of the same explanation differing only by a
/// name, which is the repetition that makes a report unreadable.
fn unjudged_note(branches: &[String]) -> Option<String> {
    if branches.is_empty() {
        return None;
    }
    let count = if branches.len() == 1 {
        "1 branch".to_owned()
    } else {
        format!("{} branches", branches.len())
    };
    Some(format!(
        "cannot tell whether {count} landed, because local differs from origin there, so \
         replaying would judge content the pull request does not contain: {}. Fetch or push, \
         then run status again",
        branches.join(", ")
    ))
}

fn note_fetched_heads(report: &mut Report, fetched_heads: usize) {
    if fetched_heads > 0 {
        report.notes.push(format!(
            "{fetched_heads} fetched pull request head(s) not listed"
        ));
    }
}

/// Every notch in this repository's ledger, read once for the whole report.
///
/// One local file read per repository rather than one per branch. A ledger that
/// exists and cannot be read is an unanswered question rather than an absence:
/// a report that quietly showed no breadcrumbs would say this fork's history was
/// never written.
fn notches_from_ledger(ledger: Option<&Ledger>, report: &mut Report) -> Vec<Notch> {
    let Some(ledger) = ledger else {
        return Vec::new();
    };
    match ledger.entries() {
        Ok(entries) => entries,
        Err(error) => {
            report.problems.push(format!("ledger unavailable: {error}"));
            Vec::new()
        }
    }
}

fn repo_notches(notches: &[Notch]) -> Option<RepoNotches> {
    let last = LastNotch::of(notches.iter().filter(|notch| notch.subject.is_none()))?;
    Some(RepoNotches {
        count: last.count,
        last,
    })
}

/// Fold the release scan into a report.
///
/// Extracted from `gather` for the same reason `scan_releases` was: that function
/// sits within a few lines of the file's hundred-line limit, and the breadcrumb
/// adds to it.
fn add_releases(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    repo: &Repo,
    tips: &BookmarkTips,
    entry: &RepoEntry,
) -> anyhow::Result<()> {
    let scheme = entry.release_scheme();
    report.newest_release = crate::release_model::newest_release(
        tips,
        &scheme,
        entry.publish_remote(),
    )
    .map(|(reference, _)| reference.to_string());
    let (names, release_findings, skipped) = scan_releases(
        repo,
        &ReleaseScan {
            path: &entry.path,
            tips,
            scheme: &scheme,
            publish_remote: entry.publish_remote(),
        },
    )?;
    report.releases = names;
    findings.extend(release_findings);
    let (double_cut_findings, double_cut_notes) =
        double_cut_findings(&entry.path, tips, &scheme, entry.publish_remote())?;
    findings.extend(double_cut_findings);
    report.notes.extend(double_cut_notes);
    if skipped > 0 {
        report
            .notes
            .push(format!("{skipped} superseded release(s) not scanned"));
    }
    Ok(())
}

/// Folds claims, workspace facts, and sidecar observations into branch rows.
fn fold_claims(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    repo: &Repo,
    name: &RepoName,
    store: &Store,
    seen: &crate::seen::Seen,
) {
    let claims: Vec<crate::store::Claim> = store.claims(Some(name)).into_iter().cloned().collect();
    let wanted: BTreeSet<crate::ids::WorkspaceName> = claims
        .iter()
        .map(|claim| crate::ids::WorkspaceName::new(crate::commands::wip::workspace_for(&claim.branch)))
        .collect();
    let activity = match repo.workspace_activity(&wanted, crate::jj::MAX_ACTIVITY_OPS) {
        Ok(activity) => activity,
        Err(error) => {
            report
                .problems
                .push(format!("workspace activity unavailable: {error}"));
            crate::jj::WorkspaceActivity::default()
        }
    };
    let mut workspaces: BTreeSet<crate::ids::WorkspaceName> = match repo.workspaces() {
        Ok(rows) => rows.into_iter().map(|(workspace, _)| workspace).collect(),
        Err(error) => {
            report
                .problems
                .push(format!("workspaces unavailable: {error}"));
            BTreeSet::new()
        }
    };
    findings.extend(crate::commands::wip::overlaps(&touching(&claims)));

    for claim in &claims {
        let row = if let Some(index) = report
            .branches
            .iter()
            .position(|row| row.name.as_str() == claim.branch)
        {
            &mut report.branches[index]
        } else {
            report.branches.push(BranchRow::bare(BranchName::new(&claim.branch)));
            report.branches.last_mut().expect("row inserted")
        };
        row.claim = Some(ClaimCell {
            id: claim.owner.clone(),
            kind: claim.kind,
            since: claim.started.clone(),
            why: claim.why.clone(),
        });
        match crate::seen::last_seen(claim, &activity, seen) {
            crate::seen::LastSeen::At(timestamp) => row.last_seen = Some(timestamp.to_string()),
            crate::seen::LastSeen::NoneSinceClaim => {
                row.seen = Some(SeenWindow::NoneSinceClaim);
            }
            crate::seen::LastSeen::NoneWithinWindow => {
                row.seen = Some(SeenWindow::NoneWithinWindow);
            }
        }
    }
    for row in &mut report.branches {
        let expected = crate::ids::WorkspaceName::new(crate::commands::wip::workspace_for(
            row.name.as_str(),
        ));
        if workspaces.remove(&expected) {
            row.workspace = Some(expected.to_string());
        }
    }
    report.other_workspaces = workspaces.into_iter().map(|workspace| workspace.to_string()).collect();
}

/// Files each claim says it is touching, keyed by claim.
fn touching(claims: &[crate::store::Claim]) -> BTreeMap<String, Vec<String>> {
    claims
        .iter()
        .map(|claim| (claim.key(), claim.files.clone()))
        .collect()
}

#[derive(Clone, Copy)]
struct CarriedFindingInput<'a> {
    report: &'a Report,
    repo: &'a Repo,
    tips: &'a BookmarkTips,
    trunk: &'a str,
    scheme: &'a ReleaseScheme,
    publish_remote: &'a str,
}

/// Reports branches carried by another branch, excluding the configured trunk.
fn carried_findings(input: CarriedFindingInput<'_>) -> anyhow::Result<Vec<Finding>> {
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
fn add_branch_overlap_findings(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    entry: &RepoEntry,
    workers: usize,
) -> std::time::Duration {
    let started = std::time::Instant::now();
    let rows = &report.branches;
    let upstream_trunk = entry.upstream_trunk();
    let path = &entry.path;
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

/// Inputs that turn completed phases into the report's visible rows and findings.
struct PostPhaseInput<'a> {
    name: &'a RepoName,
    entry: &'a RepoEntry,
    repo: &'a Repo,
    store: &'a Store,
    options: &'a Options<'a>,
    tips: &'a BookmarkTips,
    notches: &'a [Notch],
    divergent_branches: &'a [BranchName],
    probe_inputs: Vec<phases::ProbeInput>,
    probe_ran: bool,
}

fn add_pull_state_findings(
    report: &Report,
    findings: &mut Vec<Finding>,
    snapshot: &crate::snapshot::CompletedSnapshot<'_>,
) {
    let states: Vec<crate::detect::pull_state::PullState<'_>> = report
        .branches
        .iter()
        .filter_map(|row| row.pr.as_ref())
        .filter_map(|pull| {
            snapshot
                .fact(pull.number)
                .map(|fact| crate::detect::pull_state::PullState {
                    number: pull.number,
                    open: fact.pull.is_open(),
                    details: &fact.details,
                })
        })
        .collect();
    findings.extend(crate::detect::pull_state::pull_state_findings(&states));
}

/// Fold completed phases into rows, findings, timings, and cache persistence.
fn fold_phase_outcome(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    timings: &mut Timings,
    input: PostPhaseInput<'_>,
    phases: &mut phases::StatusPhases<'_>,
) -> anyhow::Result<()> {
    let snapshot = phases.forge.snapshot.as_ref();
    let empty_index = PullIndex::default();
    let index = snapshot.map_or(&empty_index, crate::snapshot::CompletedSnapshot::index);
    report
        .problems
        .extend(std::mem::take(&mut phases.forge.problems));
    timings.forge = phases.forge.duration;
    timings.probes = phases.probe.duration;

    let phase = std::time::Instant::now();
    let origin_phase = phases::origin_phase(
        &input.entry.path,
        &input.probe_inputs,
        input.options.workers,
    );
    let unjudged = rows::branch_rows(
        rows::RowInput {
            name: input.name,
            store: input.store,
            probe_inputs: input.probe_inputs,
            index,
            snapshot,
            notches: input.notches,
            expected_base: input.entry.default_base(),
        },
        std::mem::take(&mut phases.probe.verdicts),
        origin_phase.relations,
        report,
        findings,
    )?;
    timings.origin_relations = phase.elapsed();

    let phase = std::time::Instant::now();
    report
        .branches
        .extend(rows::divergent_rows(
            &rows::DivergentInput {
                branches: input.divergent_branches,
                tips: input.tips,
                name: input.name,
                store: input.store,
                snapshot,
                index,
                notches: input.notches,
                expected_base: input.entry.default_base(),
            },
            findings,
        ));
    report
        .branches
        .sort_by(|left, right| left.name.cmp(&right.name));
    timings.divergent_rows = phase.elapsed();

    let phase = std::time::Instant::now();
    let scheme = input.entry.release_scheme();
    findings.extend(carried_findings(CarriedFindingInput {
        report,
        repo: input.repo,
        tips: input.tips,
        trunk: input.entry.trunk(),
        scheme: &scheme,
        publish_remote: input.entry.publish_remote(),
    })?);
    timings.carried_findings = phase.elapsed();

    timings.touching =
        add_branch_overlap_findings(report, findings, input.entry, input.options.workers);

    let phase = std::time::Instant::now();
    let seen = crate::seen::load();
    fold_claims(report, findings, input.repo, input.name, input.store, &seen);
    timings.claims = phase.elapsed();

    let phase = std::time::Instant::now();
    report.problems.extend(unjudged_note(&unjudged));
    add_dependency_findings(DependencyInput {
        report,
        findings,
        name: input.name,
        store: input.store,
        options: input.options,
        snapshot,
        timings,
    });
    if let Some(snapshot) = snapshot {
        add_pull_state_findings(report, findings, snapshot);
    }
    if let Some(snapshot) = snapshot {
        let landed = input
            .probe_ran
            .then(|| std::mem::take(&mut phases.probe.landed));
        if let Err(note) = snapshot.persist(landed) {
            report.notes.push(note.to_string());
        }
    }
    report.forge = ForgeStatus {
        consulted: snapshot.is_some(),
        elapsed_ms: timings.forge.as_millis() as u64,
    };
    timings.report = phase.elapsed();
    Ok(())
}

/// The report, and where the run spent its time.
///
/// One function rather than two paths, so a measured run and an unmeasured one
/// cannot drift: `gather` is this with the measurement dropped.
pub fn gather_timed(
    name: &RepoName,
    entry: &RepoEntry,
    store: &Store,
    options: &Options<'_>,
) -> anyhow::Result<(Report, Timings)> {
    let started = std::time::Instant::now();
    let mut timings = Timings::default();
    let phase = std::time::Instant::now();
    let repo = Repo::open(&entry.path)?;
    let mut report = Report {
        repo: name.to_string(),
        trunk: entry.trunk().to_owned(),
        ..Report::default()
    };
    let mut findings = Vec::new();
    let tips = repo.bookmark_tips()?;
    timings.repository = phase.elapsed();
    let phase = std::time::Instant::now();
    add_releases(&mut report, &mut findings, &repo, &tips, entry)?;
    timings.releases = phase.elapsed();

    let phase = std::time::Instant::now();
    let (branches, fetched_heads) =
        rows::maintained_branches(&tips, entry.trunk(), &entry.release_scheme());
    let divergent_branches = rows::divergent_branch_names(&repo, entry)?;
    let probe_inputs = phases::probe_inputs(branches, &tips);
    let mut all_branches: Vec<BranchName> = probe_inputs
        .iter()
        .map(|input| input.branch.clone())
        .collect();
    all_branches.extend(divergent_branches.iter().cloned());
    let declared = phases::declared_numbers(name, &all_branches, store);
    let notches = notches_from_ledger(options.ledger, &mut report);
    report.repo_notches = repo_notches(&notches);
    note_fetched_heads(&mut report, fetched_heads);
    timings.setup = phase.elapsed();

    let forge_started = std::time::Instant::now();
    let opened = match phases::open_forge_snapshot(options.forge, entry, options.cache) {
        Ok(opened) => opened,
        Err(error) => {
            report
                .problems
                .push(format!("pull request state unavailable: {error}"));
            None
        }
    };
    let opened_ref = opened.as_ref();
    let trunk_commit = repo.resolve_commit(&entry.upstream_trunk()).ok();
    let probe_ran = options.probe && trunk_commit.is_some();
    let (mut phases, health) = std::thread::scope(|scope| {
        let health =
            scope.spawn(|| phases::repository_health(&entry.path, &tips, entry.publish_remote()));
        let phases = phases::run_status_phases(phases::StatusPhaseInput {
            entry,
            options,
            probe_inputs: &probe_inputs,
            opened: opened_ref,
            trunk_commit: trunk_commit.as_ref(),
            branches: &all_branches,
            declared: &declared,
            forge_started,
        });
        let health = health
            .join()
            .map_err(|_| anyhow::anyhow!("repository health phase panicked"))??;
        Ok::<_, anyhow::Error>((phases, health))
    })?;
    timings.divergent_changes = health.divergent_changes;
    timings.health = health.health;
    findings.splice(0..0, health.findings);
    report.problems.splice(0..0, health.problems);
    fold_phase_outcome(
        &mut report,
        &mut findings,
        &mut timings,
        PostPhaseInput {
            name,
            entry,
            repo: &repo,
            store,
            options,
            tips: &tips,
            notches: &notches,
            divergent_branches: &divergent_branches,
            probe_inputs,
            probe_ran,
        },
        &mut phases,
    )?;
    report.findings = group_findings(findings);
    timings.total = started.elapsed();
    Ok((report, timings))
}

pub fn gather(
    name: &RepoName,
    entry: &RepoEntry,
    store: &Store,
    options: &Options<'_>,
) -> anyhow::Result<Report> {
    gather_timed(name, entry, store, options).map(|(report, _)| report)
}

/// Which releases are worth checking for stale parents.
///
/// Not all of them. A fork accumulates every dated release it ever cut, and
/// those are frozen history: reporting stale parents on a release from ten days
/// ago is noise that buries the one finding that matters. Scanning a real
/// repository unfiltered produced twenty releases and forty-nine findings.
///
/// The rule: every local release bookmark, because those are the ones we can
/// re-cut, plus the newest remote one, because that is what a consumer is
/// plausibly pinning. Dated names sort correctly as strings. `@git` refs are
/// excluded outright: they are jj's internal git-tracking view, not a remote.
/// The count of what was skipped is reported rather than silently dropped.
/// Under `Fixed` this is instead exactly the local branch and its publish-remote counterpart: there is no accumulated history to skip, so nothing is superseded.
fn releases_to_scan(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> (Vec<(BookmarkRef, CommitId)>, usize) {
    match scheme {
        ReleaseScheme::Dated => {
            let all: Vec<(&BookmarkRef, &CommitId)> = tips
                .iter()
                .filter(|(reference, _)| is_our_release(reference, scheme, publish_remote))
                .collect();

            let newest = |local: bool| {
                all.iter()
                    .filter(|(reference, _)| reference.is_local() == local)
                    .max_by_key(|(reference, _)| release_order(reference.branch().as_str()))
                    .map(|(reference, _)| (*reference).clone())
            };
            // Only the newest cut on each side. Every local release a fork ever cut used to
            // be scanned, and their parents have all moved on by definition, so the report
            // filled with stale-parent findings for releases nothing pins: 47 of 89 in a real
            // repository, nearly all against cuts a fortnight old. The remedy attached to a stale
            // parent is to re-cut the release onto current tips, which is right for the release in
            // use and wrong for frozen history, where the answer is to forget it.
            let newest_local = newest(true);
            let newest_remote = newest(false);

            let chosen: Vec<(BookmarkRef, CommitId)> = all
                .iter()
                .filter(|(reference, _)| {
                    newest_local.as_ref() == Some(*reference)
                        || newest_remote.as_ref() == Some(*reference)
                })
                .map(|(reference, commit)| ((*reference).clone(), (*commit).clone()))
                .collect();

            let skipped = all.len() - chosen.len();
            (chosen, skipped)
        }
        ReleaseScheme::Fixed(branch) => {
            // Fixed releases advance in place, so only their local and published positions matter.
            let references = [
                BookmarkRef::Local(branch.clone()),
                BookmarkRef::Remote {
                    branch: branch.clone(),
                    remote: crate::ids::RemoteName::new(publish_remote),
                },
            ];
            let chosen = references
                .into_iter()
                .filter_map(|reference| {
                    tips.get(&reference)
                        .cloned()
                        .map(|commit| (reference, commit))
                })
                .collect();
            (chosen, 0)
        }
    }
}

/// Short form for display. Full ids are correct and unreadable.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Folds raw findings once, after every detector has reported, preserving detector order.
fn group_findings(findings: Vec<Finding>) -> Vec<FindingGroup> {
    let mut groups: Vec<FindingGroup> = Vec::new();
    for finding in findings {
        if let Some(group) = groups.iter_mut().find(|group| group.kind == finding.kind) {
            group.count += 1;
            if group.subjects.len() < 8 {
                group.subjects.push(finding.subject.short());
            }
        } else {
            groups.push(FindingGroup {
                kind: finding.kind,
                count: 1,
                subjects: vec![finding.subject.short()],
            });
        }
    }
    groups
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

/// Row and bookmark literals shared by status's split test modules.
#[cfg(test)]
pub(super) mod test_fixtures {
    use super::*;
    use crate::ids::RemoteName;

    pub(super) fn local(name: &str) -> BookmarkRef {
        BookmarkRef::Local(BranchName::new(name))
    }

    pub(super) fn remote(name: &str, remote: &str) -> BookmarkRef {
        BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(remote),
        }
    }

    pub(super) fn tips(entries: &[(BookmarkRef, &str)]) -> BookmarkTips {
        entries
            .iter()
            .map(|(reference, commit)| (reference.clone(), CommitId::new(*commit)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::test_fixtures::{local, remote, tips};
    use super::*;

    #[test]
    fn the_report_serializes_problems_before_branches_and_skips_absent_values() {
        let report = Report {
            repo: "a".into(),
            trunk: "main".into(),
            problems: vec!["pull request state unavailable: boom".into()],
            branches: vec![BranchRow::bare(BranchName::new("feat/bare"))],
            ..Report::default()
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let problems_at = json.find("\"problems\"").expect("problems present");
        let branches_at = json.find("\"branches\"").expect("branches present");
        assert!(problems_at < branches_at, "problems must lead: {json}");
        assert!(!json.contains("null"), "absent values are skipped, never null: {json}");
        assert!(
            !json.contains("\"claims\""),
            "the standalone claims section is dead: {json}"
        );
    }

    #[test]
    fn findings_group_by_kind_in_first_seen_order_and_cap_subjects() {
        let mut findings = vec![Finding::new(
            FindingKind::WrongBase,
            Subject::PullRequest(1),
            "first",
        )];
        findings.extend((0..30).map(|number| {
            Finding::new(
                FindingKind::Divergence,
                Subject::Branch(BranchName::new(format!("feat/{number}"))),
                "detail",
            )
        }));

        let groups = group_findings(findings);

        assert_eq!(groups.len(), 2, "was: {groups:?}");
        assert_eq!(groups[0].kind, FindingKind::WrongBase);
        assert_eq!(groups[1].kind, FindingKind::Divergence);
        assert_eq!(groups[1].count, 30);
        assert_eq!(groups[1].subjects.len(), 8);
        assert_eq!(groups[1].subjects[0], "feat/0");
        assert_eq!(groups[1].subjects[7], "feat/7");
    }

    #[test]
    fn a_timing_line_names_every_phase_it_measured() {
        let timings = Timings {
            repository: std::time::Duration::from_millis(4),
            health: std::time::Duration::from_millis(10),
            divergent_changes: std::time::Duration::from_millis(11),
            releases: std::time::Duration::from_millis(12),
            setup: std::time::Duration::from_millis(5),
            forge: std::time::Duration::from_millis(3400),
            probes: std::time::Duration::from_millis(8100),
            origin_relations: std::time::Duration::from_millis(16),
            divergent_rows: std::time::Duration::from_millis(17),
            carried_findings: std::time::Duration::from_millis(18),
            touching: std::time::Duration::from_millis(19),
            claims: std::time::Duration::from_millis(20),
            report: std::time::Duration::from_millis(6),
            total: std::time::Duration::from_millis(11_600),
        };
        let line = timings.line("a-repo");
        for phase in [
            "repository-open 4ms",
            "health 10ms",
            "divergent-changes 11ms",
            "releases 12ms",
            "setup 5ms",
            "forge 3400ms",
            "probes 8100ms",
            "origin-relations 16ms",
            "divergent-rows 17ms",
            "carried-findings 18ms",
            "touching 19ms",
            "claims 20ms",
            "report 6ms",
            "total 11600ms",
        ] {
            assert!(line.contains(phase), "was: {line}");
        }
    }

    #[test]
    fn a_ledger_that_cannot_be_read_is_an_unanswered_question_not_an_absence() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("a-repo");
        std::fs::create_dir_all(&path).expect("ledger directory");
        std::fs::write(
            path.join("20260815T221403.000000000Z-0000.md"),
            "not a ledger entry at all\n",
        )
        .expect("corrupt ledger");
        let ledger = crate::ledger::Ledger::at(path);
        let mut report = Report::default();

        assert!(notches_from_ledger(Some(&ledger), &mut report).is_empty());
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn report_notch_text_is_collapsed_and_capped_at_120_characters() {
        let note = Notch {
            ts: "2026-08-15T22:14:01Z".to_owned(),
            owner: "ses_fff688".to_owned(),
            subject: Some("feat/alpha".to_owned()),
            kind: crate::ledger::Kind::Note,
            disposition: Some("ruled-out".to_owned()),
            text: format!("first\n{}", "x".repeat(140)),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
        };

        let last = LastNotch::of([&note].into_iter()).expect("a notch");

        assert!(!last.text.contains('\n'), "was: {}", last.text);
        assert_eq!(last.text.chars().count(), 121, "was: {}", last.text);
        assert!(last.text.ends_with('…'), "was: {}", last.text);
    }

    #[test]
    fn release_scanning_selects_the_newest_local_and_origin_releases() {
        let map = tips(&[
            (local("release/2026-07-17.5"), "aaa"),
            (local("release/2026-07-28"), "bbb"),
            (remote("release/2026-07-20", "origin"), "ccc"),
            (remote("release/2026-07-29", "origin"), "ddd"),
        ]);

        let (chosen, skipped) = releases_to_scan(&map, &ReleaseScheme::Dated, "origin");
        let names: Vec<String> = chosen.iter().map(|(reference, _)| reference.to_string()).collect();

        assert_eq!(names, vec!["release/2026-07-28", "release/2026-07-29@origin"]);
        assert_eq!(skipped, 2);
    }

    #[test]
    fn fixed_release_scanning_uses_only_the_local_and_publish_remote_positions() {
        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let map = tips(&[
            (local("integration"), "aaa"),
            (remote("integration", "origin"), "bbb"),
            (remote("integration", "release"), "ccc"),
        ]);

        let (chosen, skipped) = releases_to_scan(&map, &fixed, "origin");
        let names: Vec<String> = chosen.iter().map(|(reference, _)| reference.to_string()).collect();

        assert_eq!(names, vec!["integration", "integration@origin"]);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn a_branch_without_a_single_tip_is_noted_but_does_not_make_status_incomplete() {
        let mut report = Report {
            branches: vec![BranchRow::bare(BranchName::new("feat/divergent"))],
            ..Report::default()
        };
        let entry = RepoEntry {
            path: std::path::PathBuf::new(),
            upstream: String::new(),
            origin: String::new(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        };

        let _ = add_branch_overlap_findings(&mut report, &mut Vec::new(), &entry, 1);

        assert!(report.notes.iter().any(|note| note.contains("no single tip")));
        assert_ne!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn a_branch_whose_path_diff_errors_is_unanswered() {
        let scratch = tempfile::tempdir().expect("temporary non-repository");
        let mut row = BranchRow::bare(BranchName::new("feat/unresolvable"));
        row.tip = Some("0700338c".to_owned());
        let mut report = Report {
            branches: vec![row],
            ..Report::default()
        };
        let entry = RepoEntry {
            path: scratch.path().to_owned(),
            upstream: String::new(),
            origin: String::new(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        };

        let _ = add_branch_overlap_findings(&mut report, &mut Vec::new(), &entry, 1);

        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn problems_and_grouped_findings_drive_the_existing_exit_contract() {
        let blocked = Report {
            problems: vec!["pull request state unavailable".to_owned()],
            ..Report::default()
        };
        assert_eq!(exit_for(&blocked), Exit::Incomplete);

        let dirty = Report {
            findings: vec![FindingGroup {
                kind: FindingKind::Divergence,
                count: 1,
                subjects: vec!["feat/a".to_owned()],
            }],
            ..Report::default()
        };
        assert_eq!(exit_for(&dirty), Exit::Findings);
    }
}
