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
use crate::ledger::{Entry as Notch, Ledger, newest_for};
use crate::store::Store;

use crate::ids::{RELEASE_PREFIX, is_our_release};

pub mod phases;
pub mod render;
mod rows;

#[derive(Debug, Clone, serde::Serialize)]
pub struct BranchRow {
    pub name: BranchName,
    /// `None` when the local bookmark is divergent, which is not a single commit.
    ///
    /// Divergent branches used to be absent from this list entirely: `bookmark_tips`
    /// only yields non-conflicted targets, so a divergent bookmark produced no row, and
    /// with no row there was no pull request association either. The pull request read
    /// as nonexistent until somebody happened to resolve the divergence. Proven by
    /// before-and-after on #228.
    pub tip: Option<CommitId>,
    /// Where origin has this branch, if anywhere. The design asks for both tips
    /// because "is my work pushed" is otherwise unanswerable, and a release cut
    /// from origin ships what origin has, not what is local.
    pub origin_tip: Option<CommitId>,
    /// How local relates to origin, when they differ and it could be determined. `None`
    /// means the tips match, there is no origin ref, or ancestry could not be resolved —
    /// and in that last case a problem is recorded, so the report says so rather than
    /// implying a relation.
    pub origin_relation: Option<OriginRelation>,
    pub pull_request: Option<PullRequest>,
    pub landed: Option<LandedVerdict>,
    pub review_stale: Option<bool>,
    /// What the forge's checks say, when they were asked for. `None` means not consulted —
    /// which is not the same as nothing having run, and must not render as a failure.
    pub checks: Option<ChecksSummary>,
    pub fork_only: bool,
    /// A pull request stated for this branch rather than inferred, with whatever
    /// state the forge reports. Inference only ever sees open pull requests from our
    /// own copy of the repository; a closed or foreign one has to be stated.
    pub stated_pull: Option<StatedPull>,
    /// The newest ledger entry about this branch, when it has one.
    ///
    /// A local file read the tool already sits beside, and the difference between
    /// a reader running one more command and a reader concluding a branch was
    /// never explained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_notch: Option<LastNotch>,
    /// This branch's other pull requests, shadowed by the primary one.
    ///
    /// A head branch accumulates pull requests over its life — an org-fork
    /// submission closed and re-homed onto a personal fork keeps its review
    /// history on the closed number — and hiding them is how an audit walked
    /// past a maintainer's blocking question that lived on a closed
    /// predecessor.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prior_pulls: Vec<PriorPull>,
}

/// A shadowed pull request, compact enough for a row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PriorPull {
    pub number: u64,
    pub state: String,
}

impl BranchRow {
    const fn bare(name: BranchName, tip: Option<CommitId>) -> Self {
        Self {
            name,
            tip,
            origin_tip: None,
            origin_relation: None,
            pull_request: None,
            landed: None,
            review_stale: None,
            checks: None,
            fork_only: false,
            stated_pull: None,
            last_notch: None,
            prior_pulls: Vec::new(),
        }
    }
}

/// How local relates to origin when the two differ.
///
/// Named states rather than a boolean because history has four cases and a boolean has
/// two: ahead, behind, forked, and could-not-tell. Collapsing the last two into a
/// boolean's `None` reported a fork as `(behind)`, which is the conflation this type
/// exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OriginRelation {
    /// Local carries commits origin does not: unpushed work.
    Ahead,
    /// Origin carries commits local does not, so a replay judges content the pull request
    /// does not contain.
    Behind,
    /// Neither tip is reachable from the other. Usual cause is a rewrite after a push.
    Diverged,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StatedPull {
    pub number: u64,
    /// Whatever the forge said, including `CLOSED`, or `unknown` when it would not say.
    pub state: String,
}

/// The part of a ledger entry a branch row carries.
///
/// Three fields, not the entry: a row is not the place to re-print an owner, an
/// anchor and a list of evidence that `knives notch <branch>` shows in full.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastNotch {
    pub ts: String,
    pub kind: crate::ledger::Kind,
    pub text: String,
}

impl LastNotch {
    fn of(entry: &Notch) -> Self {
        Self {
            ts: entry.ts.clone(),
            kind: entry.kind,
            text: entry.text.clone(),
        }
    }
}

/// The repo-scoped portion of its ledger: facts about the repository rather
/// than a branch, which therefore have no branch-row cell to carry them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepoNotches {
    pub count: usize,
    pub last: LastNotch,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    /// Who is working on what here, and since when.
    ///
    /// This used to be a separate `wip` command. Who holds a branch is part of the
    /// state of a repository, so it belongs in the one command that reports that.
    pub claims: Vec<crate::store::Claim>,
    pub workspaces: BTreeMap<String, Vec<(crate::ids::WorkspaceName, crate::ids::ChangeId)>>,
    pub repo: String,
    pub findings: Vec<Finding>,
    pub branches: Vec<BranchRow>,
    pub releases: Vec<String>,
    /// Repo-scoped ledger entries, absent when the repository has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_notches: Option<RepoNotches>,
    /// Informational: something worth saying that is not a failure.
    pub notes: Vec<String>,
    /// Could not answer. These, and only these, make the command exit non-zero
    /// for incompleteness. Keying on every note instead would make a routine
    /// remark like "14 superseded releases not scanned" look like a failure.
    pub problems: Vec<String>,
    /// Whether pull request state was actually fetched. Not having looked is not
    /// the same as having looked and found nothing, and conflating them produced
    /// a false finding for every branch.
    pub forge_consulted: bool,
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
    name: &'a RepoName,
    store: &'a Store,
    options: &'a Options<'forge>,
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    timings: &'a mut Timings,
}

fn add_dependency_findings(input: DependencyInput<'_, '_, '_>) {
    let DependencyInput {
        report,
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
    report.findings.extend(found);
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
    let mut count = 0;
    let mut last = None;
    for notch in notches {
        if notch.subject.is_none() {
            count += 1;
            last = Some(LastNotch::of(notch));
        }
    }
    last.map(|last| RepoNotches { count, last })
}

/// Fold the release scan into a report.
///
/// Extracted from `gather` for the same reason `scan_releases` was: that function
/// sits within a few lines of the file's hundred-line limit, and the breadcrumb
/// adds to it.
fn add_releases(
    report: &mut Report,
    repo: &Repo,
    tips: &BookmarkTips,
    entry: &RepoEntry,
) -> anyhow::Result<()> {
    // Releases are scanned local AND remote: what a consumer pins is the remote
    // ref, and scanning only local silently skipped the actually-pinned release.
    let (names, findings, skipped) = scan_releases(
        repo,
        &ReleaseScan {
            path: &entry.path,
            tips,
            scheme: &entry.release_scheme(),
            publish_remote: entry.publish_remote(),
        },
    )?;
    report.releases = names;
    report.findings.extend(findings);
    if skipped > 0 {
        report
            .notes
            .push(format!("{skipped} superseded release(s) not scanned"));
    }
    Ok(())
}

/// Who holds what here, and where they are working.
///
/// Absorbed from the old `wip` command. Who holds a branch is part of a repository's
/// state, so it belongs in the one command that reports that rather than in a second
/// command nobody remembers the name of.
fn add_claims(report: &mut Report, repo: &Repo, name: &RepoName, store: &Store) {
    report.claims = store.claims(Some(name)).into_iter().cloned().collect();
    if let Ok(spaces) = repo.workspaces() {
        let _ = report
            .workspaces
            .insert(name.to_string(), spaces.into_iter().collect());
    }
    report
        .findings
        .extend(crate::commands::wip::overlaps(&touching(&report.claims)));
}

/// Files each claim says it is touching, keyed by claim.
fn touching(claims: &[crate::store::Claim]) -> BTreeMap<String, Vec<String>> {
    claims
        .iter()
        .map(|claim| (claim.key(), claim.files.clone()))
        .collect()
}

/// Reports branches carried by another branch, excluding the configured trunk.
fn carried_findings(
    report: &Report,
    repo: &Repo,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for row in &report.branches {
        let Some(tip) = row.tip.as_ref() else {
            continue;
        };
        if row.landed == Some(LandedVerdict::InTrunk) {
            continue;
        }
        let carriers = repo
            .branches_containing(tip, scheme)?
            .into_iter()
            // A same-named upstream ref can contain commits ahead of ours, but this
            // deliberately treats every same-named ref as the branch itself.
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
    report
        .findings
        .extend(crate::detect::overlap::branch_overlaps(&touching));
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

fn add_pull_state_findings(report: &mut Report, snapshot: &crate::snapshot::CompletedSnapshot<'_>) {
    let states: Vec<crate::detect::pull_state::PullState<'_>> = report
        .branches
        .iter()
        .filter_map(|row| row.pull_request.as_ref())
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
    report
        .findings
        .extend(crate::detect::pull_state::pull_state_findings(&states));
}

/// Fold completed phases into rows, findings, timings, and cache persistence.
fn fold_phase_outcome(
    report: &mut Report,
    timings: &mut Timings,
    input: PostPhaseInput<'_>,
    phases: &mut phases::StatusPhases<'_>,
) -> anyhow::Result<()> {
    let snapshot = phases.forge.snapshot.as_ref();
    let empty_index = PullIndex::default();
    let index = snapshot.map_or(&empty_index, crate::snapshot::CompletedSnapshot::index);
    report.forge_consulted = snapshot.is_some();
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
        },
        std::mem::take(&mut phases.probe.verdicts),
        origin_phase.relations,
        report,
    )?;
    timings.origin_relations = phase.elapsed();

    let phase = std::time::Instant::now();
    report
        .branches
        .extend(rows::divergent_rows(&rows::DivergentInput {
            branches: input.divergent_branches,
            tips: input.tips,
            name: input.name,
            store: input.store,
            snapshot,
            index,
            notches: input.notches,
        }));
    report
        .branches
        .sort_by(|left, right| left.name.cmp(&right.name));
    timings.divergent_rows = phase.elapsed();

    let phase = std::time::Instant::now();
    report.findings.extend(carried_findings(
        report,
        input.repo,
        input.entry.trunk(),
        &input.entry.release_scheme(),
    )?);
    timings.carried_findings = phase.elapsed();

    timings.touching = add_branch_overlap_findings(report, input.entry, input.options.workers);

    let phase = std::time::Instant::now();
    add_claims(report, input.repo, input.name, input.store);
    timings.claims = phase.elapsed();

    let phase = std::time::Instant::now();
    report.findings.extend(branch_findings(&report.branches));
    report.findings.extend(wrong_base_findings(
        &report.branches,
        input.entry.default_base(),
    ));
    report.problems.extend(unjudged_note(&unjudged));
    add_dependency_findings(DependencyInput {
        report,
        name: input.name,
        store: input.store,
        options: input.options,
        snapshot,
        timings,
    });
    if let Some(snapshot) = snapshot {
        add_pull_state_findings(report, snapshot);
    }
    if let Some(snapshot) = snapshot {
        let landed = input
            .probe_ran
            .then(|| std::mem::take(&mut phases.probe.landed));
        if let Err(note) = snapshot.persist(landed) {
            report.notes.push(note.to_string());
        }
    }
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
        ..Report::default()
    };
    let tips = repo.bookmark_tips()?;
    timings.repository = phase.elapsed();
    let phase = std::time::Instant::now();
    add_releases(&mut report, &repo, &tips, entry)?;
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
        let health = scope.spawn(|| phases::repository_health(&entry.path, &tips));
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
    report.findings.splice(0..0, health.findings);
    report.problems.splice(0..0, health.problems);
    fold_phase_outcome(
        &mut report,
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

/// Order a dated release name so numeric suffixes compare numerically.
///
/// String order is wrong here: `release/2026-07-28.10` sorts BELOW
/// `release/2026-07-28.2` because "1" < "2". The tenth repair of one day then
/// silently audits the wrong release. Returns the date part and the suffix as
/// separate comparable pieces.
pub fn release_order(name: &str) -> (String, u32) {
    let bare = name.strip_prefix(RELEASE_PREFIX).unwrap_or(name);
    match bare.split_once('.') {
        Some((date, suffix)) => (date.to_owned(), suffix.parse().unwrap_or(0)),
        None => (bare.to_owned(), 0),
    }
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
                .filter(|(reference, _)| is_our_release(reference, scheme))
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

/// Observations about branches that the branch line itself cannot carry.
///
/// Once the advice was removed, most of what lived here went with it: whether a branch
/// is in the trunk, and whether it has a pull request, are facts already stated on the
/// branch's own line. Repeating them as findings only added volume.
fn branch_findings(rows: &[BranchRow]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for row in rows {
        // The forge's own verdict, not a guess. A pull request in conflict with its base
        // looks complete from every other angle — tests green, review approved, nothing
        // left to write — and cannot be merged; an agent called one ready to ship while it
        // was in conflict with main.
        if let Some(pr) = row.pull_request.as_ref()
            && pr.conflicting()
        {
            findings.push(Finding::new(
                FindingKind::Unmergeable,
                Subject::PullRequest(pr.number),
                format!(
                    "#{} cannot be merged as it stands: the forge reports {} ({})",
                    pr.number,
                    pr.mergeable.to_lowercase(),
                    if pr.merge_state_status.is_empty() {
                        "no further detail".to_owned()
                    } else {
                        pr.merge_state_status.to_lowercase()
                    }
                ),
            ));
        }
        if let (Some(pr), Some(checks)) = (row.pull_request.as_ref(), row.checks.as_ref())
            && pr.is_open()
            && checks.failing()
        {
            findings.push(Finding::new(
                FindingKind::ChecksFailing,
                Subject::PullRequest(pr.number),
                format!(
                    "#{} has failing checks: {}",
                    pr.number,
                    checks.failed_names().join(", ")
                ),
            ));
        }
        // Only when a pull request exists. The old default rendered a finding
        // for `#0`, a fabricated identifier, rather than declining to speak.
        if let (Some(true), Some(pr)) = (row.review_stale, row.pull_request.as_ref()) {
            let number = pr.number;
            findings.push(Finding::new(
                FindingKind::StaleReview,
                Subject::PullRequest(number),
                format!(
                    "the newest review on #{number} predates the newest commit on {}",
                    row.name
                ),
            ));
        }
    }
    findings
}

/// Open pull requests whose base branch name differs from the configured base.
///
/// An empty base means the forge did not say, which is not the same as wrong.
fn wrong_base_findings(rows: &[BranchRow], expected: &str) -> Vec<Finding> {
    rows.iter()
        .filter_map(|row| {
            let pr = row.pull_request.as_ref()?;
            if !pr.is_open() || pr.base_ref_name.is_empty() || pr.base_ref_name == expected {
                return None;
            }
            Some(Finding::new(
                FindingKind::WrongBase,
                Subject::PullRequest(pr.number),
                format!(
                    "#{} targets {}, not {expected}",
                    pr.number, pr.base_ref_name
                ),
            ))
        })
        .collect()
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

/// Row and bookmark literals shared by the split test modules; scenario tests
/// state only the fields under test.
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

    pub(super) fn row(
        name: &str,
        landed: Option<LandedVerdict>,
        pr: Option<PullRequest>,
    ) -> BranchRow {
        BranchRow {
            pull_request: pr,
            landed,
            ..BranchRow::bare(BranchName::new(name), Some(CommitId::new("0700338c")))
        }
    }

    pub(super) fn pull_request(number: u64) -> PullRequest {
        PullRequest {
            number,
            review_decision: "APPROVED".to_owned(),
            head_ref_name: "feat/alpha".to_owned(),
            head_ref_oid: "deadbeef".to_owned(),
            ..PullRequest::default()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::test_fixtures::{local, pull_request, remote, row, tips};
    use super::*;
    use crate::ids::BranchName;

    #[test]
    fn a_timing_line_names_every_phase_it_measured() {
        // The numbers this PR is judged against. A line that reported only a total
        // could not say which phase a change actually moved.
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
        assert!(line.contains("a-repo"), "was: {line}");
        assert!(line.contains("repository-open 4ms"), "was: {line}");
        assert!(line.contains("health 10ms"), "was: {line}");
        assert!(line.contains("divergent-changes 11ms"), "was: {line}");
        assert!(line.contains("releases 12ms"), "was: {line}");
        assert!(line.contains("setup 5ms"), "was: {line}");
        assert!(line.contains("forge 3400ms"), "was: {line}");
        assert!(line.contains("probes 8100ms"), "was: {line}");
        assert!(line.contains("origin-relations 16ms"), "was: {line}");
        assert!(line.contains("divergent-rows 17ms"), "was: {line}");
        assert!(line.contains("carried-findings 18ms"), "was: {line}");
        assert!(line.contains("touching 19ms"), "was: {line}");
        assert!(line.contains("claims 20ms"), "was: {line}");
        assert!(line.contains("report 6ms"), "was: {line}");
        assert!(line.contains("total 11600ms"), "was: {line}");
    }

    #[test]
    fn a_ledger_that_cannot_be_read_is_an_unanswered_question_not_an_absence() {
        // A report that quietly showed no breadcrumbs would say this fork's
        // history was never written.
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

        let notches = notches_from_ledger(Some(&ledger), &mut report);

        assert!(notches.is_empty());
        assert_eq!(report.problems.len(), 1, "was: {report:?}");
        assert!(report.problems[0].contains("ledger"), "was: {report:?}");
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn only_the_newest_release_on_each_side_is_scanned() {
        // A fork accumulates every release it ever cut, and every one of their parents
        // has moved on, so scanning them all filled the report with stale-parent
        // findings for releases nothing pins: 47 of 89 on a real repository. Two local
        // releases here on purpose; with one, this test cannot tell "every local" from
        // "the newest local".
        let map = tips(&[
            (local("release/2026-07-17.5"), "aaa"),
            (local("release/2026-07-28"), "bbb"),
            (remote("release/2026-07-20", "origin"), "ccc"),
            (remote("release/2026-07-29", "origin"), "ddd"),
            (local("feat/alpha"), "eee"),
        ]);
        let (chosen, skipped) = releases_to_scan(&map, &ReleaseScheme::Dated, "origin");
        let names: Vec<String> = chosen.iter().map(|(r, _)| r.to_string()).collect();
        assert!(names.contains(&"release/2026-07-28".to_owned()));
        assert!(names.contains(&"release/2026-07-29@origin".to_owned()));
        assert!(
            !names.contains(&"release/2026-07-17.5".to_owned()),
            "a superseded local release is frozen history: {names:?}"
        );
        assert!(!names.contains(&"release/2026-07-20@origin".to_owned()));
        assert_eq!(skipped, 2, "what was skipped must be reported, not dropped");
    }

    #[test]
    fn an_upstream_release_is_never_a_candidate() {
        // We did not cut it and cannot re-cut it. It also has a different date
        // format, which sorted above ours and won the "newest" slot until this
        // was fixed.
        let map = tips(&[
            (local("release/2026-07-28"), "aaa"),
            (
                remote("release/20260416144609+gcloud-fix", "upstream"),
                "zzz",
            ),
        ]);
        let (chosen, _) = releases_to_scan(&map, &ReleaseScheme::Dated, "origin");
        assert!(
            chosen
                .iter()
                .all(|(r, _)| !r.to_string().contains("upstream"))
        );
    }

    #[test]
    fn only_releases_we_cut_count_as_ours() {
        // `repos` and `status` each grew their own version of this, and `repos`
        // promptly picked an upstream release as ours. One predicate now.
        assert!(is_our_release(
            &local("release/2026-07-29"),
            &ReleaseScheme::Dated
        ));
        assert!(is_our_release(
            &remote("release/2026-07-29", "origin"),
            &ReleaseScheme::Dated
        ));
        assert!(is_our_release(
            &remote("release/2026-07-29", "release"),
            &ReleaseScheme::Dated
        ));
        assert!(!is_our_release(
            &remote("release/2026-07-29", "upstream"),
            &ReleaseScheme::Dated
        ));
        assert!(!is_our_release(
            &remote("release/2026-07-29", "git"),
            &ReleaseScheme::Dated
        ));
        assert!(!is_our_release(&local("feat/alpha"), &ReleaseScheme::Dated));
    }

    #[test]
    fn a_double_digit_repair_suffix_sorts_above_a_single_digit_one() {
        // String order puts `.10` below `.2`, so from the tenth repair of one
        // day onward the wrong release is audited and the difference is
        // reported as "1 superseded release not scanned".
        assert!(release_order("release/2026-07-28.10") > release_order("release/2026-07-28.2"));
        assert!(release_order("release/2026-07-29") > release_order("release/2026-07-28.10"));
        assert!(release_order("release/2026-07-28.1") > release_order("release/2026-07-28"));
    }

    #[test]
    fn the_newest_release_is_chosen_numerically_not_lexically() {
        let map = tips(&[
            (remote("release/2026-07-28.2", "origin"), "aaa"),
            (remote("release/2026-07-28.10", "origin"), "bbb"),
        ]);
        let (chosen, _) = releases_to_scan(&map, &ReleaseScheme::Dated, "origin");
        let names: Vec<String> = chosen.iter().map(|(r, _)| r.to_string()).collect();
        assert_eq!(names, vec!["release/2026-07-28.10@origin".to_owned()]);
    }

    #[test]
    fn jj_internal_git_refs_are_not_releases() {
        let map = tips(&[(remote("release/2026-07-29", "git"), "aaa")]);
        let (chosen, _) = releases_to_scan(&map, &ReleaseScheme::Dated, "origin");
        assert!(chosen.is_empty());
    }

    #[test]
    fn under_a_fixed_scheme_the_fixed_branch_is_scanned_and_is_not_a_maintained_branch() {
        // Given: a fixed release branch, its publish remote, a different release-role remote,
        // and a dated leftover from before the scheme changed.
        let fixed = crate::ids::ReleaseScheme::Fixed(BranchName::new("integration"));
        let map = tips(&[
            (local("integration"), "aaa"),
            (remote("integration", "origin"), "bbb"),
            (remote("integration", "release"), "eee"),
            (local("feat/alpha"), "ccc"),
            (local("release/2026-07-28"), "ddd"),
        ]);
        // When: releases are chosen and branches collected under the fixed scheme.
        let (chosen, skipped) = releases_to_scan(&map, &fixed, "origin");
        let names: Vec<String> = chosen
            .iter()
            .map(|(reference, _)| reference.to_string())
            .collect();
        let (branches, _) = rows::maintained_branches(&map, "main", &fixed);
        let branch_names: Vec<String> = branches
            .iter()
            .map(|(branch, _)| branch.to_string())
            .collect();
        // Then: local and publish-remote positions are the complete scan set, while the fixed
        // branch is a cut rather than carried work.
        assert!(names.contains(&"integration".to_owned()), "was: {names:?}");
        assert!(
            names.contains(&"integration@origin".to_owned()),
            "was: {names:?}"
        );
        assert!(
            !names.contains(&"integration@release".to_owned()),
            "only the publish remote's counterpart is a release candidate: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.contains("release/")),
            "was: {names:?}"
        );
        assert_eq!(skipped, 0);
        assert!(
            !branch_names.contains(&"integration".to_owned()),
            "was: {branch_names:?}"
        );
        assert!(
            branch_names.contains(&"release/2026-07-28".to_owned()),
            "was: {branch_names:?}"
        );
    }

    #[test]
    fn the_dated_scheme_still_scans_only_the_newest_release_on_each_side() {
        // Given: dated releases with one superseded local cut.
        let map = tips(&[
            (local("release/2026-07-17.5"), "aaa"),
            (local("release/2026-07-28"), "bbb"),
            (remote("release/2026-07-29", "origin"), "ddd"),
        ]);
        // When: releases are chosen under the dated scheme.
        let (chosen, skipped) = releases_to_scan(&map, &ReleaseScheme::Dated, "origin");
        // Then: exactly the latest local and remote cuts are kept.
        assert_eq!(chosen.len(), 2);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn a_branch_without_a_single_tip_is_noted_but_does_not_make_status_incomplete() {
        // Given: a divergent bookmark row, which cannot name one commit to compare
        let mut report = Report {
            branches: vec![BranchRow::bare(BranchName::new("feat/divergent"), None)],
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

        // When: overlap paths are gathered alongside the landed probe
        let _ = add_branch_overlap_findings(&mut report, &entry, 1);

        // Then: the known divergence is announced without claiming the report is incomplete.
        assert!(report.notes.iter().any(|note| {
            note.contains("cannot compare paths for feat/divergent")
                && note.contains("no single tip")
        }));
        assert!(report.problems.is_empty(), "was: {report:?}");
        assert_ne!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn a_branch_whose_path_diff_errors_is_unanswered() {
        // Given: a reported branch and a repository path where jj cannot resolve its range
        let scratch = tempfile::tempdir().expect("temporary non-repository");
        let mut report = Report {
            branches: vec![BranchRow::bare(
                BranchName::new("feat/unresolvable"),
                Some(CommitId::new("0700338c")),
            )],
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

        // When: jj cannot calculate that branch's changed paths
        let _ = add_branch_overlap_findings(&mut report, &entry, 1);

        // Then: the report says path coverage was incomplete instead of silently omitting it
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("cannot compare paths for feat/unresolvable"))
        );
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn a_pull_request_in_conflict_with_its_base_is_reported() {
        // The case this exists for: a pull request that looks finished from every other
        // angle — tests green, review approved, nothing left to write — and cannot be
        // merged. An agent called one code complete and ready to ship while it was in
        // conflict with main. Four of thirteen open pull requests were in that state.
        let mut pr = pull_request(4565);
        pr.mergeable = "CONFLICTING".to_owned();
        pr.merge_state_status = "DIRTY".to_owned();
        let findings = branch_findings(&[row("feat/alpha", None, Some(pr))]);

        let found = findings
            .iter()
            .find(|finding| finding.kind == FindingKind::Unmergeable)
            .expect("a conflicting pull request must be reported");
        assert!(found.detail.contains("#4565"), "was: {}", found.detail);
        assert!(
            found.detail.contains("conflicting"),
            "was: {}",
            found.detail
        );
    }

    #[test]
    fn a_pull_request_against_the_wrong_base_is_reported() {
        let mut wrong = pull_request(21);
        wrong.base_ref_name = "release/2026-07-28".to_owned();
        let findings = wrong_base_findings(&[row("feat/alpha", None, Some(wrong))], "main");
        let found = findings
            .iter()
            .find(|finding| finding.kind == FindingKind::WrongBase)
            .expect("a wrong base must be reported");
        assert!(
            found.detail.contains("release/2026-07-28"),
            "was: {}",
            found.detail
        );
        assert!(
            found.detail.contains("main"),
            "name the expected base: {}",
            found.detail
        );

        let mut right = pull_request(22);
        right.base_ref_name = "main".to_owned();
        assert!(wrong_base_findings(&[row("feat/beta", None, Some(right))], "main").is_empty());

        let mut merged = pull_request(23);
        merged.state = "MERGED".to_owned();
        merged.base_ref_name = "release/2026-07-28".to_owned();
        assert!(wrong_base_findings(&[row("feat/gamma", None, Some(merged))], "main").is_empty());

        // Unknown is not wrong: an empty base means the forge did not say.
        let mut quiet = pull_request(24);
        quiet.base_ref_name.clear();
        assert!(wrong_base_findings(&[row("feat/gamma", None, Some(quiet))], "main").is_empty());
    }

    #[test]
    fn a_mergeable_pull_request_is_not_reported_and_neither_is_an_unknown_one() {
        // The forge computes mergeability asynchronously, so treating "not worked out yet"
        // as "broken" would cry wolf on every fresh push.
        let mut fine = pull_request(1);
        fine.mergeable = "MERGEABLE".to_owned();
        let mut unknown = pull_request(2);
        unknown.mergeable = "UNKNOWN".to_owned();
        for pr in [fine, unknown] {
            let findings = branch_findings(&[row("feat/alpha", None, Some(pr))]);
            assert!(
                !findings
                    .iter()
                    .any(|finding| finding.kind == FindingKind::Unmergeable),
                "only CONFLICTING is a conflict: {findings:?}"
            );
        }
    }

    #[test]
    fn failing_checks_are_reported_and_an_empty_rollup_is_not() {
        // Given: one pull request with red CI and one without a reported check
        let mut red = row("feat/alpha", None, Some(pull_request(11)));
        red.checks = Some(crate::forge::ChecksSummary {
            runs: vec![crate::forge::CheckRun {
                name: "build".to_owned(),
                conclusion: Some("FAILURE".to_owned()),
            }],
        });

        // When: branch findings are derived
        let findings = branch_findings(&[red]);
        let found = findings
            .iter()
            .find(|finding| finding.kind == FindingKind::ChecksFailing)
            .expect("a failing check must be reported");

        // Then: the failing check is named, while an empty rollup has no finding
        assert!(
            found.detail.contains("build"),
            "name the check: {}",
            found.detail
        );
        let mut quiet = row("feat/beta", None, Some(pull_request(12)));
        quiet.checks = Some(crate::forge::ChecksSummary::default());
        assert!(
            !branch_findings(&[quiet])
                .iter()
                .any(|finding| finding.kind == FindingKind::ChecksFailing)
        );
    }

    #[test]
    fn a_stale_review_names_the_pull_request_and_says_to_re_read() {
        let mut stale = row("feat/alpha", None, Some(pull_request(42)));
        stale.review_stale = Some(true);
        let findings = branch_findings(&[stale]);
        let review = findings
            .iter()
            .find(|f| f.kind == FindingKind::StaleReview)
            .expect("finding");
        assert_eq!(review.subject.to_string(), "#42");
    }

    #[test]
    fn a_problem_means_incomplete_even_when_there_are_no_findings() {
        // A CI gate on `knives status` must not go green because the forge login
        // failed and every detector therefore found nothing.
        let blocked = Report {
            repo: "a".to_owned(),
            problems: vec!["pull request state unavailable".to_owned()],
            ..Report::default()
        };
        assert_eq!(exit_for(&blocked), Exit::Incomplete);
    }

    #[test]
    fn an_informational_note_does_not_make_the_command_look_broken() {
        // "14 superseded releases not scanned" is a remark, not a failure.
        // Keying incompleteness on every note made status always exit 3.
        let chatty = Report {
            repo: "a".to_owned(),
            notes: vec!["14 superseded release(s) not scanned".to_owned()],
            ..Report::default()
        };
        assert_eq!(exit_for(&chatty), Exit::Ok);
    }

    #[test]
    fn findings_make_the_command_exit_non_zero_so_a_script_can_gate_on_it() {
        let clean = Report {
            repo: "a".to_owned(),
            ..Report::default()
        };
        assert_eq!(exit_for(&clean), Exit::Ok);
        let dirty = Report {
            repo: "a".to_owned(),
            findings: vec![Finding::new(
                FindingKind::Divergence,
                Subject::File("a".to_owned()),
                "d",
            )],
            ..Report::default()
        };
        assert_eq!(exit_for(&dirty), Exit::Findings);
    }
}
