//! `knives status`: per-branch state and all four detectors against a live repo.

use std::collections::BTreeMap;
use std::fmt;

use crate::cli::Exit;
use crate::config::{Registry, RepoEntry, Role};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, LandedVerdict, Subject, classify_landed, divergent_changes,
    double_checkout, stale_parents,
};
use crate::forge::{ChecksSummary, Forge, PullDetails, PullRequest, ours_only};
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, ReleaseScheme, RepoName, is_release_name,
    pull_number_from_bookmark,
};
use crate::jj::{JjError, Repo, branches_past, probe_landed};
use crate::ledger::{Entry as Notch, Ledger, newest_for};
use crate::store::Store;

pub use crate::ids::{RELEASE_PREFIX, is_our_release};

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
    /// Scanning releases for stale parents.
    pub releases: std::time::Duration,
    /// The pull request list, batched details, and stated-pull state lookups for
    /// maintained branches. Divergent-row stated-pull and dependency lookups are
    /// not counted.
    pub forge: std::time::Duration,
    /// Replaying branches onto the upstream trunk.
    pub probes: std::time::Duration,
    pub total: std::time::Duration,
}

impl Timings {
    pub fn line(&self, repo: &str) -> String {
        format!(
            "timing {repo}: releases {}ms forge {}ms probes {}ms total {}ms",
            self.releases.as_millis(),
            self.forge.as_millis(),
            self.probes.as_millis(),
            self.total.as_millis()
        )
    }
}

/// Whether phase timings were asked for.
pub fn timing_enabled() -> bool {
    std::env::var_os("KNIVES_TIMING").is_some()
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

/// Whether the branch has landed upstream, or that the question cannot be answered.
///
/// Judge only what the pull request actually contains. The probe replays the local
/// bookmark, so when local and origin disagree it answers about content nobody has
/// pushed, and stale content replays clean and reads as landed. Refusing to judge is
/// cheap; the `landed` advice is to delete the branch and its release parent.
#[allow(
    clippy::too_many_arguments,
    reason = "the approved interface separately names the probe inputs and configured upstream trunk"
)]
fn landed_verdict(
    path: &std::path::Path,
    branch: &BranchName,
    // Local tip, and the origin tip when the branch has been pushed.
    tips: (&CommitId, Option<&CommitId>),
    options: &Options<'_>,
    upstream_trunk: &str,
) -> Result<Option<LandedVerdict>, JjError> {
    let (tip, origin_tip) = tips;
    if !options.probe {
        return Ok(None);
    }
    if origin_tip.is_some_and(|origin| origin != tip) {
        return Ok(Some(LandedVerdict::Unjudged));
    }
    Ok(Some(classify_landed(probe_landed(
        path,
        branch,
        upstream_trunk,
    )?)))
}
/// Landed verdicts for every branch, probed concurrently, in branch order.
///
/// Each probe opens its own repository handle and replays inside a transaction it
/// drops: nothing is shared between threads and nothing is written, so no probe
/// can observe another's. Verified against jj-lib rather than assumed — see
/// `jj_lib_answers_the_same_probe_from_many_threads_as_from_one`.
///
/// Bounded by chunking the branch list rather than by a work queue, because the
/// bound is the point and a queue would be a dependency. Results come back in the
/// order the branches went in, so this is the serial report.
fn landed_verdicts(
    path: &std::path::Path,
    carried: &[CarriedPull],
    options: &Options<'_>,
    upstream_trunk: &str,
) -> Vec<Result<Option<LandedVerdict>, JjError>> {
    // Nothing to spawn for: `landed_verdict` answers `None` without probing when
    // the probe is off, and an empty list has no chunks.
    if !options.probe || carried.is_empty() {
        return carried.iter().map(|_| Ok(None)).collect();
    }
    let workers = options.workers.clamp(1, carried.len());
    let chunk = carried.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for slice in carried.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|(branch, tip, origin_tip, _)| {
                            landed_verdict(
                                path,
                                branch,
                                (tip, origin_tip.as_ref()),
                                options,
                                upstream_trunk,
                            )
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
                        .map(|(branch, ..)| {
                            Err(JjError::ProbePanic {
                                branch: branch.to_string(),
                            })
                        })
                        .collect()
                })
            })
            .collect()
    })
}

/// The pull request a bookmark refers to, by branch name or by fetched-head number.
///
/// A `pr-<n>` bookmark is a fetched pull request head: its name is the number, not
/// the branch the pull request came from, so matching on name alone never found one.
fn pull_request_for(
    branch: &BranchName,
    open: &BTreeMap<BranchName, PullRequest>,
) -> Option<PullRequest> {
    open.get(branch).cloned().or_else(|| {
        let number = pull_number_from_bookmark(branch.as_str())?;
        open.values().find(|pr| pr.number == number).cloned()
    })
}

/// Divergent bookmarks, named individually.
///
/// A divergent branch's bookmark is conflicted, so it has no single tip and never
/// appears in the branch list. Naming it here is the difference between "some change
/// is divergent" and "this branch of yours is".
fn conflicted_bookmark_findings(repo: &Repo) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for (reference, commits) in repo.conflicted_bookmarks()? {
        let shown: Vec<String> = commits.iter().map(|c| short(c.as_str())).collect();
        findings.push(Finding::new(
            FindingKind::Divergence,
            Subject::Bookmark(reference.clone()),
            format!(
                "bookmark {reference} points at {} commits ({}), so it has no single tip",
                commits.len(),
                shown.join(", ")
            ),
        ));
    }
    Ok(findings)
}

/// Declared dependencies that are not satisfied yet.
///
/// A branch can require a pull request in a sibling fork. Dropping the required one
/// from a release without dropping the branch that needs it ships a release that
/// cannot work, which is exactly what happened when one repo's #4545 was dropped
/// while a sibling's #49 still needed it. Satisfied means merged: an open pull
/// request may still change or be rejected.
struct DependencyContext<'a> {
    store: &'a Store,
    registry: &'a Registry,
    forge: Option<&'a dyn Forge>,
}

fn unmet_dependencies(
    repo: &RepoName,
    branches: &[BranchRow],
    context: &DependencyContext<'_>,
) -> (Vec<Finding>, Vec<String>) {
    let DependencyContext {
        store,
        registry,
        forge,
    } = *context;
    let mut findings = Vec::new();
    let mut problems = Vec::new();
    for row in branches {
        let target = BranchTarget::new(repo.clone(), row.name.clone());
        for requirement in store.dependencies(&target) {
            let Some(entry) = registry.get(&requirement.repo) else {
                problems.push(format!(
                    "{} requires {requirement}, whose repo is not in the registry",
                    row.name
                ));
                continue;
            };
            let Some(forge) = forge else {
                problems.push(format!(
                    "cannot check whether {} still needs {requirement}: no forge consulted",
                    row.name
                ));
                continue;
            };
            match forge.pull_request_state(&entry.path, requirement.number) {
                Ok(Some(state)) if state.eq_ignore_ascii_case("MERGED") => {}
                Ok(Some(state)) => findings.push(Finding::new(
                    FindingKind::UnmetDependency,
                    Subject::Branch(row.name.clone()),
                    format!(
                        "{} requires {requirement}, which is {}",
                        row.name,
                        state.to_lowercase()
                    ),
                )),
                Ok(None) => problems.push(format!(
                    "{} requires {requirement}, which the forge did not report on",
                    row.name
                )),
                Err(error) => problems.push(format!(
                    "cannot check whether {} still needs {requirement}: {error}",
                    row.name
                )),
            }
        }
    }
    (findings, problems)
}

/// Fold declared cross-repo requirements into a report.
///
/// Separate from `gather` only to keep that function readable.
fn add_dependency_findings(
    report: &mut Report,
    name: &RepoName,
    store: &Store,
    options: &Options<'_>,
) {
    let Some(registry) = options.registry else {
        return;
    };
    let (found, unanswered) = unmet_dependencies(
        name,
        &report.branches,
        &DependencyContext {
            store,
            registry,
            forge: options.forge,
        },
    );
    report.findings.extend(found);
    report.problems.extend(unanswered);
}

/// Branches we maintain apart from the configured trunk, and fetched pull request heads skipped.
///
/// A `pr-<n>` bookmark is not a branch of ours: it is a pull request head this tool
/// fetched so a release could carry it. Treating fetch artifacts as our work is most of
/// why this report was unreadable — on one repository they were 16 of 28 rows and 10 of
/// 24 findings, every one of the latter advising us to drop a branch that was never
/// ours. They also each cost a landed probe, which is most of the runtime.
fn maintained_branches(
    tips: &BookmarkTips,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> (Vec<(BranchName, CommitId)>, usize) {
    let mut fetched_heads = 0_usize;
    let branches = tips
        .iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch)
                if !is_release_name(branch, scheme) && branch.as_str() != trunk =>
            {
                if pull_number_from_bookmark(branch.as_str()).is_some() {
                    fetched_heads += 1;
                    return None;
                }
                Some((branch.clone(), commit.clone()))
            }
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        })
        .collect();
    (branches, fetched_heads)
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

/// The pull request stated for a branch, with whatever state the forge reports.
fn stated_pull_for(
    target: &BranchTarget,
    store: &Store,
    entry: &RepoEntry,
    options: &Options<'_>,
) -> Option<StatedPull> {
    store.tracked_pull(target).map(|number| StatedPull {
        state: options
            .forge
            .and_then(|forge| forge.pull_request_state(&entry.path, number).ok())
            .flatten()
            .unwrap_or_else(|| "unknown".to_owned()),
        number,
    })
}

fn pull_requests_from_forge(
    forge: Option<&dyn Forge>,
    entry: &RepoEntry,
    report: &mut Report,
) -> BTreeMap<BranchName, PullRequest> {
    let Some(forge) = forge else {
        return BTreeMap::new();
    };
    match forge.pull_requests(&entry.path) {
        Ok(found) => {
            report.forge_consulted = true;
            // Ours means it comes from our copy of the repository, not that its branch
            // happens to share a name with one of ours.
            ours_only(
                found,
                &[entry.remote(Role::Origin), entry.remote(Role::Release)],
            )
        }
        Err(error) => {
            report
                .problems
                .push(format!("pull request state unavailable: {error}"));
            BTreeMap::new()
        }
    }
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

/// One maintained branch, its tip, where origin has it, and the pull request it
/// refers to.
type CarriedPull = (BranchName, CommitId, Option<CommitId>, Option<PullRequest>);

/// Every branch paired with what the row loop needs before it starts.
///
/// Built up front because both phases that dominate a run — the forge round trip
/// and the landed probes — go over the whole list at once now, and a loop that
/// discovers its own inputs one at a time is exactly what made them serial.
fn carried_pulls(
    branches: Vec<(BranchName, CommitId)>,
    pull_requests: &BTreeMap<BranchName, PullRequest>,
    tips: &BookmarkTips,
) -> Vec<CarriedPull> {
    branches
        .into_iter()
        .map(|(branch, tip)| {
            let origin_tip = tips
                .get(&BookmarkRef::Remote {
                    branch: branch.clone(),
                    remote: crate::ids::RemoteName::new("origin"),
                })
                .cloned();
            let pull_request = pull_request_for(&branch, pull_requests);
            (branch, tip, origin_tip, pull_request)
        })
        .collect()
}

/// The pull requests worth asking the forge about.
///
/// Exactly the ones the per-branch calls asked about: a review age only when the
/// forge recorded a review decision, checks only while the pull request is open.
/// Asking about more would be a behaviour change dressed as an optimisation.
fn detail_numbers(carried: &[CarriedPull]) -> Vec<u64> {
    let mut numbers: Vec<u64> = carried
        .iter()
        .filter_map(|(_, _, _, pull_request)| pull_request.as_ref())
        .filter(|pull_request| pull_request.is_open() || !pull_request.review_decision.is_empty())
        .map(|pull_request| pull_request.number)
        .collect();
    // Sorted and duplicate-free to keep the query shape stable without redundant
    // fields. No two rows can name one number today — `maintained_branches` drops
    // the `pr-<n>` bookmarks that are the only other way to reach a number — and
    // this keeps the query's shape independent of that staying true.
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// Review age and check state for every pull request in this report, in one call.
fn pull_details_from_forge(
    forge: Option<&dyn Forge>,
    entry: &RepoEntry,
    numbers: &[u64],
    report: &mut Report,
) -> BTreeMap<u64, PullDetails> {
    let Some(forge) = forge else {
        return BTreeMap::new();
    };
    if numbers.is_empty() {
        return BTreeMap::new();
    }
    match forge.pull_details(&entry.path, numbers) {
        Ok(details) => details,
        Err(error) => {
            report
                .problems
                .push(format!("review age and checks unavailable: {error}"));
            BTreeMap::new()
        }
    }
}

/// Whether the newest review predates the branch head, when there was a review to
/// compare.
///
/// Gated as the per-pull-request call was: an empty review decision means the
/// forge recorded no review, and `None` must never render as "current".
fn review_stale_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<bool> {
    let pull_request = pull_request?;
    if pull_request.review_decision.is_empty() {
        return None;
    }
    details?.review_predates_head
}

/// What the forge's checks say, for an open pull request that was consulted.
///
/// Settled pull requests are not asked about and not reported on: a closed one's
/// recorded rollup is obsolete the moment it closes.
fn checks_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<ChecksSummary> {
    let pull_request = pull_request?;
    if !pull_request.is_open() {
        return None;
    }
    details?.checks.clone()
}

struct DivergentInput<'a> {
    repo: &'a Repo,
    tips: &'a BookmarkTips,
    name: &'a RepoName,
    entry: &'a RepoEntry,
    store: &'a Store,
    options: &'a Options<'a>,
    pull_requests: &'a BTreeMap<BranchName, PullRequest>,
    notches: &'a [Notch],
}

/// Rows for divergent local bookmarks.
///
/// `bookmark_tips` cannot report these: a conflicted target has no single commit, so
/// jj-lib yields nothing for it. Without them these branches were absent from the
/// listing entirely, and a branch with no row got no pull request association either, so
/// its pull request read as nonexistent until somebody happened to resolve the
/// divergence. Proven by before-and-after on #228.
fn divergent_rows(input: &DivergentInput<'_>) -> anyhow::Result<Vec<BranchRow>> {
    let mut rows = Vec::new();
    let scheme = input.entry.release_scheme();
    for (reference, _) in input.repo.conflicted_bookmarks()? {
        let BookmarkRef::Local(branch) = reference else {
            continue;
        };
        if is_release_name(&branch, &scheme)
            || branch.as_str() == input.entry.trunk()
            || pull_number_from_bookmark(branch.as_str()).is_some()
        {
            continue;
        }
        let target = BranchTarget::new(input.name.clone(), branch.clone());
        let row = BranchRow {
            fork_only: input.store.is_fork_only(&target),
            stated_pull: stated_pull_for(&target, input.store, input.entry, input.options),
            pull_request: pull_request_for(&branch, input.pull_requests),
            origin_tip: input
                .tips
                .get(&BookmarkRef::Remote {
                    branch: branch.clone(),
                    remote: crate::ids::RemoteName::new("origin"),
                })
                .cloned(),
            last_notch: newest_for(input.notches, branch.as_str()).map(LastNotch::of),
            // Nothing to replay: a divergent bookmark has no single commit to probe.
            ..BranchRow::bare(branch, None)
        };
        rows.push(row);
    }
    Ok(rows)
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

/// How local relates to origin when that relation can be determined.
///
/// Two ancestry queries, not one: `is_ancestor(origin, tip)` returning false covers both
/// "origin is ahead" and "the histories forked", and reporting a fork as `(behind)` tells a
/// reader to trust origin over their own unpushed work. The second query separates them.
///
/// A failure comes back as an error rather than a relation, because a relation this could not
/// establish is not a fact about history — the same reason `LandedVerdict::Unjudged` exists.
pub fn relation_to_origin(
    repo: &Repo,
    tip: &CommitId,
    origin_tip: Option<&CommitId>,
) -> Result<Option<OriginRelation>, JjError> {
    match origin_tip {
        Some(origin) if origin != tip => {
            if repo.is_ancestor(origin, tip)? {
                Ok(Some(OriginRelation::Ahead))
            } else if repo.is_ancestor(tip, origin)? {
                Ok(Some(OriginRelation::Behind))
            } else {
                Ok(Some(OriginRelation::Diverged))
            }
        }
        Some(_) | None => Ok(None),
    }
}

fn record_origin_relation(
    report: &mut Report,
    branch: &BranchName,
    relation: Result<Option<OriginRelation>, JjError>,
) -> Option<OriginRelation> {
    match relation {
        Ok(relation) => relation,
        Err(error) => {
            report.problems.push(format!(
                "cannot tell how {branch} relates to origin: {error}"
            ));
            None
        }
    }
}

fn record_repository_health(
    report: &mut Report,
    repo: &Repo,
    path: &std::path::Path,
    tips: &BookmarkTips,
) -> anyhow::Result<()> {
    // Recorded before conclusions from this repository: detectors replay commits, so a stale
    // working copy can invalidate their answers.
    if let Some(stale) = repo.stale_working_copy(path) {
        report.problems.push(stale);
    }
    report.findings.extend(double_checkout(&repo.workspaces()?));
    let ignored: std::collections::BTreeSet<crate::ids::BookmarkRef> =
        crate::commands::release::superseded_dated_releases(tips)
            .into_iter()
            .map(|(reference, _)| reference)
            .collect();
    report
        .findings
        .extend(divergent_changes(&repo.divergent_changes(&ignored)?));
    report.findings.extend(conflicted_bookmark_findings(repo)?);
    Ok(())
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

fn add_branch_overlap_findings(report: &mut Report, entry: &RepoEntry) {
    let mut touching: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut notes = Vec::new();
    let mut unanswered = Vec::new();
    for row in &report.branches {
        if row.tip.is_none() {
            notes.push(format!(
                "cannot compare paths for {}: it has no single tip",
                row.name
            ));
            continue;
        }
        let from = format!("fork_point({} | {})", entry.upstream_trunk(), row.name);
        match crate::jj::changed_files_between(&entry.path, &from, row.name.as_str()) {
            Ok(files) => {
                let _ = touching.insert(row.name.to_string(), files);
            }
            Err(error) => {
                unanswered.push(format!("cannot compare paths for {}: {error}", row.name));
            }
        }
    }
    report.notes.extend(notes);
    report.problems.extend(unanswered);
    report
        .findings
        .extend(crate::detect::overlap::branch_overlaps(&touching));
}

/// Everything the branch table needs from one repository.
struct RowInput<'a> {
    name: &'a RepoName,
    entry: &'a RepoEntry,
    repo: &'a Repo,
    tips: &'a BookmarkTips,
    store: &'a Store,
    options: &'a Options<'a>,
    branches: Vec<(BranchName, CommitId)>,
    pull_requests: &'a BTreeMap<BranchName, PullRequest>,
    notches: &'a [Notch],
    upstream_trunk: &'a str,
}

/// The branch rows, and the branches whose landed state could not be judged.
///
/// Extracted from `gather` because the two phases that dominate a status run —
/// the forge round trips and the landed probes — are driven over the whole branch
/// list, and one function that both drives them and assembles the rest of a
/// report is past what a reviewer holds at once.
fn branch_rows(
    input: RowInput<'_>,
    report: &mut Report,
    timings: &mut Timings,
) -> anyhow::Result<Vec<String>> {
    let carried = carried_pulls(input.branches, input.pull_requests, input.tips);

    let phase = std::time::Instant::now();
    let details = pull_details_from_forge(
        input.options.forge,
        input.entry,
        &detail_numbers(&carried),
        report,
    );
    timings.forge += phase.elapsed();

    let phase = std::time::Instant::now();
    let verdicts = landed_verdicts(
        &input.entry.path,
        &carried,
        input.options,
        input.upstream_trunk,
    );
    timings.probes = phase.elapsed();

    let mut unjudged = Vec::new();
    for (verdict, (branch, tip, origin_tip, pull_request)) in verdicts.into_iter().zip(carried) {
        // Propagated in branch order, so a probe failure reports the same branch
        // and the same message it did when the probes ran one at a time.
        let landed = verdict?;
        if landed == Some(LandedVerdict::Unjudged) {
            unjudged.push(branch.to_string());
        }
        let detail = pull_request
            .as_ref()
            .and_then(|pull_request| details.get(&pull_request.number));
        let review_stale = review_stale_from(detail, pull_request.as_ref());
        let checks = checks_from(detail, pull_request.as_ref());
        let origin_relation = record_origin_relation(
            report,
            &branch,
            relation_to_origin(input.repo, &tip, origin_tip.as_ref()),
        );
        let target = BranchTarget::new(input.name.clone(), branch.clone());
        let phase = std::time::Instant::now();
        let stated_pull = stated_pull_for(&target, input.store, input.entry, input.options);
        timings.forge += phase.elapsed();
        let last_notch = newest_for(input.notches, branch.as_str()).map(LastNotch::of);
        report.branches.push(BranchRow {
            name: branch,
            tip: Some(tip),
            origin_tip,
            origin_relation,
            pull_request,
            landed,
            review_stale,
            checks,
            fork_only: input.store.is_fork_only(&target),
            stated_pull,
            last_notch,
        });
    }
    Ok(unjudged)
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
    let repo = Repo::open(&entry.path)?;
    let mut report = Report {
        repo: name.to_string(),
        ..Report::default()
    };
    let tips = repo.bookmark_tips()?;
    record_repository_health(&mut report, &repo, &entry.path, &tips)?;
    let trunk = entry.trunk();
    let scheme = entry.release_scheme();
    let upstream_trunk = entry.upstream_trunk();
    let phase = std::time::Instant::now();
    add_releases(&mut report, &repo, &tips, entry)?;
    timings.releases = phase.elapsed();

    let (branches, fetched_heads) = maintained_branches(&tips, trunk, &scheme);
    let notches = notches_from_ledger(options.ledger, &mut report);
    report.repo_notches = repo_notches(&notches);
    note_fetched_heads(&mut report, fetched_heads);
    let phase = std::time::Instant::now();
    let pull_requests = pull_requests_from_forge(options.forge, entry, &mut report);
    timings.forge += phase.elapsed();

    let unjudged = branch_rows(
        RowInput {
            name,
            entry,
            repo: &repo,
            tips: &tips,
            store,
            options,
            branches,
            pull_requests: &pull_requests,
            upstream_trunk: &upstream_trunk,
            notches: &notches,
        },
        &mut report,
        &mut timings,
    )?;

    report.branches.extend(divergent_rows(&DivergentInput {
        repo: &repo,
        tips: &tips,
        name,
        entry,
        store,
        options,
        pull_requests: &pull_requests,
        notches: &notches,
    })?);
    report
        .branches
        .sort_by(|left, right| left.name.cmp(&right.name));
    report
        .findings
        .extend(carried_findings(&report, &repo, trunk, &scheme)?);
    add_branch_overlap_findings(&mut report, entry);

    add_claims(&mut report, &repo, name, store);

    let derived = branch_findings(&report.branches);
    report.findings.extend(derived);
    report
        .findings
        .extend(wrong_base_findings(&report.branches, entry.default_base()));
    report.problems.extend(unjudged_note(&unjudged));
    add_dependency_findings(&mut report, name, store, options);
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

/// One line per kind of finding, naming every subject.
///
/// A finding per branch times a detector per finding made the report unreadable: one
/// repository printed 89 blocks, and a wall of text that has to be read in full to
/// find the two things that matter is the same as not being told. Nothing is dropped
/// here, only folded: every subject is named, and `--verbose` still prints each
/// finding with its own detail line.
fn grouped(findings: &[Finding]) -> Vec<String> {
    let mut order: Vec<FindingKind> = Vec::new();
    let mut by_kind: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for finding in findings {
        let key = finding.kind.to_string();
        if !order.iter().any(|kind| kind.to_string() == key) {
            order.push(finding.kind);
        }
        by_kind
            .entry(key)
            .or_default()
            .push(finding.subject.short());
    }
    let width = order.iter().map(|k| k.to_string().len()).max().unwrap_or(0);
    order
        .iter()
        .filter_map(|kind| {
            let key = kind.to_string();
            let subjects = by_kind.get(&key)?;
            // Enough subjects to act on, then a count, so one loud detector cannot push
            // the others off the screen.
            let shown = subjects
                .iter()
                .take(6)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            let rest = subjects.len().saturating_sub(6);
            let listed = if rest == 0 {
                shown
            } else {
                format!("{shown}, and {rest} more")
            };
            Some(format!(
                "    {key:<width$}  {:>3}  {listed}",
                subjects.len()
            ))
        })
        .collect()
}

/// Active claims, one block each.
fn claim_lines(claims: &[crate::store::Claim]) -> Vec<String> {
    if claims.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!("  claims      {}", claims.len())];
    for claim in claims {
        lines.push(format!(
            "    {}  {}  since {}",
            claim.branch, claim.owner, claim.started
        ));
        lines.push(format!("      {}", claim.why));
    }
    lines
}

fn branch_cell(row: &BranchRow) -> String {
    row.name.to_string()
}

fn tip_cell(row: &BranchRow) -> String {
    row.tip
        .as_ref()
        .map_or_else(|| "divergent".to_owned(), |tip| short(tip.as_str()))
}

fn push_cell(row: &BranchRow) -> String {
    match (&row.origin_tip, &row.tip) {
        (None, _) => "unpushed".to_owned(),
        (Some(origin), Some(tip)) if origin != tip => match row.origin_relation {
            Some(OriginRelation::Ahead) => "unpushed-commits".to_owned(),
            Some(OriginRelation::Behind) => format!("origin={} (behind)", short(origin.as_str())),
            Some(OriginRelation::Diverged) => {
                format!("origin={} (diverged)", short(origin.as_str()))
            }
            None => format!("origin={} (unresolved)", short(origin.as_str())),
        },
        (Some(_), _) => "pushed".to_owned(),
    }
}

fn stated_pull_cell(stated: &StatedPull) -> String {
    format!("#{} {}", stated.number, stated_pull_details(stated))
}

fn stated_pull_details(stated: &StatedPull) -> String {
    format!("{} (stated)", stated.state.to_lowercase())
}

fn pull_request_cell(row: &BranchRow) -> String {
    row.pull_request.as_ref().map_or_else(
        || {
            row.stated_pull
                .as_ref()
                .map_or_else(|| "no-pr".to_owned(), stated_pull_cell)
        },
        |pr| {
            let mut details = vec![format!("#{}", pr.number)];
            if !pr.is_open() {
                details.push(pr.state.to_lowercase());
            }
            if pr.is_draft {
                details.push("draft".to_owned());
            }
            if let Some(stated) = &row.stated_pull {
                if stated.number == pr.number {
                    details.push(stated_pull_details(stated));
                } else {
                    details.push(stated_pull_cell(stated));
                }
            }
            details.join(" ")
        },
    )
}

fn review_cell(row: &BranchRow) -> String {
    match &row.pull_request {
        Some(pr) if pr.review_decision.is_empty() => "no-review".to_owned(),
        Some(pr) => pr.review_decision.clone(),
        None => "-".to_owned(),
    }
}

fn checks_cell(row: &BranchRow) -> String {
    match row.pull_request.as_ref() {
        Some(pr) if pr.is_open() => match row.checks.as_ref() {
            Some(checks) if checks.failing() => "failing".to_owned(),
            Some(checks) if !checks.ran() => "none-ran".to_owned(),
            Some(_) => "ok".to_owned(),
            None => "-".to_owned(),
        },
        Some(_) | None => "-".to_owned(),
    }
}

fn landed_cell(row: &BranchRow) -> String {
    row.landed
        .map_or_else(|| "-".to_owned(), |verdict| verdict.to_string())
}

fn flags_cell(row: &BranchRow) -> String {
    let mut flags = Vec::new();
    if let Some(pr) = &row.pull_request {
        if pr.conflicting() {
            flags.push("CONFLICTING");
        } else if pr.merge_state_status.eq_ignore_ascii_case("BEHIND") {
            flags.push("behind-base");
        }
    }
    if row.review_stale == Some(true) {
        flags.push("review-stale");
    }
    if row.fork_only {
        flags.push("fork-only");
    }
    if flags.is_empty() {
        "-".to_owned()
    } else {
        flags.join(",")
    }
}

/// How much of a notch's text a branch line carries.
const NOTCH_TEXT: usize = 32;

/// Render a ledger entry in the one-line status form.
fn notch_summary(notch: &LastNotch) -> String {
    let collapsed = notch.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let escaped = crate::ledger::inline_human_text(&collapsed);
    let mut shown: String = escaped.chars().take(NOTCH_TEXT).collect();
    if escaped.chars().count() > NOTCH_TEXT {
        shown.push('…');
    }
    crate::ledger::age(&notch.ts, jiff::Timestamp::now()).map_or_else(
        || format!("\"{shown}\""),
        |age| format!("\"{shown}\" ({age})"),
    )
}

/// The newest notch on this branch, as one token.
///
/// Truncated and whitespace-collapsed because an entry's text is free prose that
/// may run to a paragraph and may contain newlines, and this is a table cell: one
/// stray newline destroys every column below it.
fn notch_cell(row: &BranchRow) -> String {
    row.last_notch
        .as_ref()
        .map_or_else(|| "-".to_owned(), notch_summary)
}

fn repo_notch_line(notches: &RepoNotches) -> String {
    format!(
        "  notches  {} repo-level, newest: {}",
        notches.count,
        notch_summary(&notches.last)
    )
}

fn branch_table(rows: &[BranchRow]) -> Vec<String> {
    const HEADER: [&str; 9] = [
        "branch", "tip", "push", "pr", "review", "checks", "landed", "flags", "notch",
    ];

    let cells: Vec<[String; 9]> = rows
        .iter()
        .map(|row| {
            [
                branch_cell(row),
                tip_cell(row),
                push_cell(row),
                pull_request_cell(row),
                review_cell(row),
                checks_cell(row),
                landed_cell(row),
                flags_cell(row),
                notch_cell(row),
            ]
        })
        .collect();
    let mut widths = HEADER.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let format_row = |cells: [&str; 9]| {
        let [
            branch,
            tip,
            push,
            pull_request,
            review,
            checks,
            landed,
            flags,
            notch,
        ] = cells;
        let [
            branch_width,
            tip_width,
            push_width,
            pull_request_width,
            review_width,
            checks_width,
            landed_width,
            flags_width,
            notch_width,
        ] = widths;
        format!(
            "    {branch:<branch_width$}  {tip:<tip_width$}  {push:<push_width$}  {pull_request:<pull_request_width$}  {review:<review_width$}  {checks:<checks_width$}  {landed:<landed_width$}  {flags:<flags_width$}  {notch:<notch_width$}"
        )
        .trim_end()
        .to_owned()
    };
    let mut lines = vec![format_row(HEADER)];
    lines.extend(cells.iter().map(|row| {
        let [
            branch,
            tip,
            push,
            pull_request,
            review,
            checks,
            landed,
            flags,
            notch,
        ] = row.each_ref();
        format_row([
            branch.as_str(),
            tip.as_str(),
            push.as_str(),
            pull_request.as_str(),
            review.as_str(),
            checks.as_str(),
            landed.as_str(),
            flags.as_str(),
            notch.as_str(),
        ])
    }));
    lines
}

pub fn render(report: &Report, verbose: bool) -> String {
    // The repository is named once, at the top, and everything under it is indented.
    // Prefixing every section with it repeated the name four times per repo, which over
    // ten repos is forty lines of the same word and no structure at all.
    let mut lines: Vec<String> = vec![report.repo.clone()];
    if !report.releases.is_empty() {
        lines.push(format!(
            "  releases    {} checked: {}",
            report.releases.len(),
            report.releases.join(", ")
        ));
    }
    if report.branches.is_empty() {
        lines.push("  branches    none".to_owned());
    } else {
        lines.push(format!("  branches    {}", report.branches.len()));
    }
    if let Some(notches) = &report.repo_notches {
        lines.push(repo_notch_line(notches));
    }
    if !report.branches.is_empty() {
        lines.extend(branch_table(&report.branches));
    }
    if report.findings.is_empty() {
        lines.push("  findings    none".to_owned());
    } else {
        lines.push(format!("  findings    {}", report.findings.len()));
        if verbose {
            for finding in &report.findings {
                lines.push(format!(
                    "    [{}] {}",
                    finding.kind,
                    finding.subject.short()
                ));
                lines.push(format!("      {}", finding.detail));
            }
        } else {
            lines.extend(grouped(&report.findings));
        }
    }
    // Problems decide the exit code, so printing them is not optional: a non-zero
    // exit whose reason appears nowhere in the output is a gate nobody can act on.
    if !report.problems.is_empty() {
        lines.push(format!("  unanswered  {}", report.problems.len()));
        for problem in &report.problems {
            lines.push(format!("    {problem}"));
        }
    }
    lines.extend(claim_lines(&report.claims));
    let mut notes: Vec<String> = report.notes.clone();
    if !report.forge_consulted {
        notes.push("pull request state was not checked; branch columns are unknown".to_owned());
    }
    if !notes.is_empty() {
        lines.push(format!("  notes       {}", notes.len()));
        for note in &notes {
            lines.push(format!("    {note}"));
        }
    }
    lines.join("\n")
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

#[cfg(test)]
fn render_verbose(report: &Report) -> String {
    render(report, true)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;
    use crate::ids::{BranchName, RemoteName};

    fn local(name: &str) -> BookmarkRef {
        BookmarkRef::Local(BranchName::new(name))
    }

    fn remote(name: &str, remote: &str) -> BookmarkRef {
        BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(remote),
        }
    }

    fn tips(entries: &[(BookmarkRef, &str)]) -> BookmarkTips {
        entries
            .iter()
            .map(|(reference, commit)| (reference.clone(), CommitId::new(*commit)))
            .collect()
    }

    fn row(name: &str, landed: Option<LandedVerdict>, pr: Option<PullRequest>) -> BranchRow {
        BranchRow {
            pull_request: pr,
            landed,
            ..BranchRow::bare(BranchName::new(name), Some(CommitId::new("0700338c")))
        }
    }

    fn pull_request(number: u64) -> PullRequest {
        PullRequest {
            number,
            review_decision: "APPROVED".to_owned(),
            head_ref_name: "feat/alpha".to_owned(),
            head_ref_oid: "deadbeef".to_owned(),
            ..PullRequest::default()
        }
    }
    #[test]
    fn a_timing_line_names_every_phase_it_measured() {
        // The numbers this PR is judged against. A line that reported only a total
        // could not say which phase a change actually moved.
        let timings = Timings {
            releases: std::time::Duration::from_millis(12),
            forge: std::time::Duration::from_millis(3400),
            probes: std::time::Duration::from_millis(8100),
            total: std::time::Duration::from_millis(11_600),
        };
        let line = timings.line("a-repo");
        assert!(line.contains("a-repo"), "was: {line}");
        assert!(line.contains("releases 12ms"), "was: {line}");
        assert!(line.contains("forge 3400ms"), "was: {line}");
        assert!(line.contains("probes 8100ms"), "was: {line}");
        assert!(line.contains("total 11600ms"), "was: {line}");
    }

    #[test]
    fn branch_rows_render_as_an_aligned_table_with_a_header() {
        // Vertical alignment without horizontal alignment made ten-branch reports
        // unreadable: every fact was present and nothing lined up.
        let with_pr = row(
            "feat/alpha",
            Some(LandedVerdict::InTrunk),
            Some(pull_request(1128)),
        );
        let bare = row("fix/a-much-longer-branch-name", None, None);

        let lines = branch_table(&[with_pr, bare]);

        assert_eq!(lines.len(), 3, "header plus one row per branch: {lines:?}");
        let header = &lines[0];
        assert!(header.contains("branch") && header.contains("pr") && header.contains("landed"));
        // Every row starts each column at the same offset as the header.
        let column_start = |line: &str, word: &str| line.find(word).unwrap_or(usize::MAX);
        let tip_at = column_start(header, "tip");
        for line in &lines[1..] {
            assert!(line.len() >= tip_at, "short row breaks alignment: {line:?}");
        }
        assert!(lines[1].contains("#1128"), "was: {}", lines[1]);
        assert!(lines[1].contains("APPROVED"));
        assert!(lines[2].contains("no-pr"));
        assert!(
            lines[2].contains('-'),
            "empty cells render as placeholders, not gaps"
        );
    }

    /// The offset at which each column's content begins, and the gap that precedes it.
    ///
    /// Cells never contain two consecutive spaces, so a run of two or more spaces is
    /// unambiguously a column separator plus that column's padding.
    fn columns(line: &str) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut gap = 0;
        for (offset, ch) in line.char_indices() {
            if ch == ' ' {
                gap += 1;
            } else {
                if gap >= 2 || offset == 0 {
                    found.push((offset, gap));
                }
                gap = 0;
            }
        }
        found
    }

    #[test]
    fn an_empty_cell_never_shifts_its_neighbours() {
        let with_flags = {
            let mut pr = pull_request(7);
            pr.mergeable = "CONFLICTING".to_owned();
            row("feat/conflicted", None, Some(pr))
        };
        let plain = row("feat/plain", None, None);

        let lines = branch_table(&[with_flags, plain]);

        let header_columns = columns(&lines[0]);
        let header_offsets: Vec<usize> = header_columns.iter().map(|(offset, _)| *offset).collect();
        assert_eq!(header_offsets.len(), 9, "was: {}", lines[0]);
        for line in &lines {
            assert_eq!(
                line.chars().take_while(|ch| *ch == ' ').count(),
                4,
                "was: {line}"
            );
            let row_columns = columns(line);
            let row_offsets: Vec<usize> = row_columns.iter().map(|(offset, _)| *offset).collect();
            assert_eq!(row_offsets, header_offsets, "was: {line}");
            assert_eq!(
                row_columns
                    .iter()
                    .skip(1)
                    .map(|(_, gap)| *gap)
                    .min()
                    .expect("a table row has separators"),
                2,
                "was: {line}"
            );
        }
        assert_eq!(columns(&lines[2]).len(), 9, "was: {}", lines[2]);
        assert!(lines[2].ends_with(" -"), "was: {}", lines[2]);
        assert!(lines[1].contains("CONFLICTING"));
    }

    #[test]
    fn a_branchs_newest_notch_is_one_token_at_the_end_of_its_line() {
        // Status text is already dense: the breadcrumb is one token, and its
        // legibility overhaul is separate work.
        let mut row = row("feat/log-queue", None, None);
        row.last_notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "superseded by #1157".to_owned(),
        });
        let lines = branch_table(&[row]);
        assert!(lines[0].contains("notch"), "header: {}", lines[0]);
        assert!(
            lines[1].ends_with("\"superseded by #1157\" (now)"),
            "was: {}",
            lines[1]
        );
    }

    #[test]
    fn a_long_or_multi_line_notch_cannot_break_the_table() {
        // An entry's text is free prose that may run to a paragraph and may carry
        // newlines. One stray newline destroys every column below it.
        let mut row = row("feat/alpha", None, None);
        row.last_notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "parked by the owner\nuntil the trait lands upstream, which may be weeks"
                .to_owned(),
        });
        let lines = branch_table(&[row]);
        assert_eq!(lines.len(), 2, "was: {lines:?}");
        assert!(!lines[1].contains('\n'));
        assert!(lines[1].contains('…'), "truncation is marked: {}", lines[1]);
        assert!(
            lines[1].contains("parked by the owner until"),
            "newlines collapse to spaces: {}",
            lines[1]
        );
    }

    #[test]
    fn a_notch_control_character_cannot_reach_the_status_table() {
        let mut row = row("feat/alpha", None, None);
        row.last_notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "parked\u{1b}now\ragain".to_owned(),
        });

        let lines = branch_table(&[row]);
        assert!(!lines[1].contains('\u{1b}'), "was: {:?}", lines[1]);
        assert!(!lines[1].contains('\r'), "was: {:?}", lines[1]);
        assert!(lines[1].contains('\u{fffd}'), "was: {:?}", lines[1]);
    }

    #[test]
    fn a_branch_with_no_notch_renders_the_empty_placeholder() {
        let lines = branch_table(&[row("feat/alpha", None, None)]);
        assert!(lines[1].ends_with(" -"), "was: {}", lines[1]);
        assert_eq!(columns(&lines[1]).len(), 9, "was: {}", lines[1]);
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
    fn the_trunk_exclusion_follows_the_repo_entry_not_the_name_main() {
        // Given: a repo whose upstream trunk is dev, carrying a branch named main
        let map = tips(&[
            (local("dev"), "aaa"),
            (local("main"), "bbb"),
            (local("feat/alpha"), "ccc"),
        ]);
        // When: maintained branches are collected with dev as the trunk
        let (branches, _) = maintained_branches(&map, "dev", &ReleaseScheme::Dated);
        let names: Vec<String> = branches
            .iter()
            .map(|(branch, _)| branch.to_string())
            .collect();
        // Then: dev is excluded as the trunk, and a branch that merely shares the
        // name main is ours to report
        assert!(!names.contains(&"dev".to_owned()), "was: {names:?}");
        assert!(names.contains(&"main".to_owned()), "was: {names:?}");
        assert!(names.contains(&"feat/alpha".to_owned()));
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
        let (branches, _) = maintained_branches(&map, "main", &fixed);
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
    fn a_problem_is_printed_not_just_counted_in_the_exit_code() {
        // Problems drive Exit::Incomplete. A non-zero exit whose cause appears
        // nowhere in the output cannot be acted on.
        let report = Report {
            repo: "demo".to_owned(),
            problems: vec!["cannot tell whether feat/x landed".to_owned()],
            ..Report::default()
        };
        let out = render(&report, true);
        assert!(
            out.contains("cannot tell whether feat/x landed"),
            "was: {out}"
        );
        assert!(out.contains("unanswered"), "was: {out}");
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
        add_branch_overlap_findings(&mut report, &entry);

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
        add_branch_overlap_findings(&mut report, &entry);

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
    fn a_branch_behind_origin_is_not_judged_against_the_trunk() {
        // The probe replays the local bookmark. When local is behind origin it replays
        // stale content, which comes back clean and reads as already in the trunk.
        // Observed against a real repository: two branches with open pull requests were
        // called landed while nothing of theirs was upstream.
        let note = unjudged_note(&["feat/alpha".to_owned(), "feat/beta".to_owned()]);
        let note = note.expect("two unjudgeable branches must be reported");
        assert!(note.contains("2 branches"), "was: {note}");
        assert!(note.contains("feat/alpha") && note.contains("feat/beta"));
        assert!(
            unjudged_note(&[]).is_none(),
            "nothing to say when all were judged"
        );
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
                conclusion: "FAILURE".to_owned(),
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
    fn ci_readiness_cells_preserve_draft_and_check_facts() {
        // Given: a draft with red CI and a draft whose checks have not run
        let mut failing = pull_request(11);
        failing.is_draft = true;
        let mut failing = row("feat/failing", None, Some(failing));
        failing.checks = Some(crate::forge::ChecksSummary {
            runs: vec![crate::forge::CheckRun {
                name: "build".to_owned(),
                conclusion: "FAILURE".to_owned(),
            }],
        });
        let mut never_ran = pull_request(12);
        never_ran.is_draft = true;
        let mut never_ran = row("feat/never-ran", None, Some(never_ran));
        never_ran.checks = Some(crate::forge::ChecksSummary::default());
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![failing, never_ran],
            ..Report::default()
        };

        // When: the branch rows are rendered
        let rendered = render(&report, false);
        let failing_line = rendered
            .lines()
            .find(|line| line.contains("feat/failing"))
            .expect("the failing branch line");
        let never_ran_line = rendered
            .lines()
            .find(|line| line.contains("feat/never-ran"))
            .expect("the never-ran branch line");

        // Then: each row retains its draft and CI facts in separate table cells
        assert!(
            failing_line.contains("draft") && failing_line.contains("failing"),
            "was: {failing_line}"
        );
        assert!(
            never_ran_line.contains("draft") && never_ran_line.contains("none-ran"),
            "was: {never_ran_line}"
        );
    }

    #[test]
    fn not_consulted_checks_do_not_render_as_none_ran() {
        // Given: matching pull requests whose checks were and were not consulted
        let mut no_checks = row("feat/no-checks", None, Some(pull_request(11)));
        no_checks.checks = Some(crate::forge::ChecksSummary::default());
        let not_consulted = row("feat/not-consulted", None, Some(pull_request(12)));
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![no_checks, not_consulted],
            ..Report::default()
        };

        // When: the branch rows are rendered
        let rendered = render(&report, false);
        let no_checks_line = rendered
            .lines()
            .find(|line| line.contains("feat/no-checks"))
            .expect("the consulted branch line");
        let not_consulted_line = rendered
            .lines()
            .find(|line| line.contains("feat/not-consulted"))
            .expect("the unconsulted branch line");

        // Then: the three states stay distinct
        assert!(no_checks_line.contains("none-ran"), "was: {no_checks_line}");
        assert!(
            !not_consulted_line.contains("none-ran"),
            "not consulted is not nothing-ran: {not_consulted_line}"
        );
        assert!(
            !not_consulted_line.contains("failing"),
            "not consulted is not failing: {not_consulted_line}"
        );
    }

    #[test]
    fn settled_pull_requests_do_not_report_obsolete_check_status() {
        // Given: a closed pull request whose recorded check rollup is red
        let mut pull_request = pull_request(4634);
        pull_request.state = "CLOSED".to_owned();
        let mut closed = row("feat/closed", None, Some(pull_request));
        closed.checks = Some(crate::forge::ChecksSummary {
            runs: vec![crate::forge::CheckRun {
                name: "build".to_owned(),
                conclusion: "FAILURE".to_owned(),
            }],
        });

        // When: the settled branch is rendered and analysed
        let rendered = branch_table(std::slice::from_ref(&closed)).join("\n");
        let findings = branch_findings(&[closed]);

        // Then: no action-oriented CI token or finding is emitted
        assert!(!rendered.contains("none-ran"), "was: {rendered}");
        assert!(!rendered.contains("failing"), "was: {rendered}");
        assert!(
            !findings
                .iter()
                .any(|finding| finding.kind == FindingKind::ChecksFailing),
            "was: {findings:?}"
        );
    }

    #[test]
    fn a_draft_pull_request_says_so() {
        // Already requested from the forge and already deserialised, and nothing rendered it,
        // so the cheapest "not ready" signal there is was being thrown away.
        let mut pr = pull_request(7);
        pr.is_draft = true;
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![row("feat/alpha", None, Some(pr))],
            ..Report::default()
        };

        assert!(
            render(&report, false).contains("draft"),
            "was: {}",
            render(&report, false)
        );
    }

    #[test]
    fn the_same_inferred_and_stated_pull_number_is_rendered_once_with_its_provenance() {
        // Given: an open inferred pull request and a stated record for that same pull request.
        let mut row = row("feat/alpha", None, Some(pull_request(106)));
        row.stated_pull = Some(StatedPull {
            number: 106,
            state: "OPEN".to_owned(),
        });

        // When: the pull-request cell combines inference with the stated record.
        let cell = pull_request_cell(&row);

        // Then: the number is shown once while the stated state and provenance remain visible.
        assert_eq!(cell, "#106 open (stated)");
    }

    #[test]
    fn different_inferred_and_stated_pull_numbers_are_both_rendered() {
        // Given: an inferred pull request and a distinct stated pull request.
        let mut row = row("feat/alpha", None, Some(pull_request(106)));
        row.stated_pull = Some(StatedPull {
            number: 107,
            state: "OPEN".to_owned(),
        });

        // When: the pull-request cell combines inference with the stated record.
        let cell = pull_request_cell(&row);

        // Then: both numbers remain visible because they identify different pull requests.
        assert_eq!(cell, "#106 #107 open (stated)");
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
    fn a_branch_whose_origin_is_ahead_is_shown_as_behind() {
        // "Is my work pushed" is otherwise unanswerable, and it decides whether
        // a release cut from origin ships the current code.
        let mut row = row("feat/alpha", None, None);
        row.origin_tip = Some(CommitId::new("deadbeefdead"));
        row.origin_relation = Some(OriginRelation::Behind);
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row],
            forge_consulted: true,
            ..Report::default()
        });
        assert!(out.contains("behind"), "was: {out}");
    }

    #[test]
    fn local_ahead_of_origin_is_not_reported_as_behind_it() {
        // One word for both directions was a live bug: unpushed local work and a local copy
        // that is stale read identically, and only one of them invalidates a landed verdict.
        let mut ahead = row("feat/alpha", None, None);
        ahead.tip = Some(CommitId::new("aaaaaaaaaaaa"));
        ahead.origin_tip = Some(CommitId::new("bbbbbbbbbbbb"));
        ahead.origin_relation = Some(OriginRelation::Ahead);
        let report = Report {
            repo: "demo".to_owned(),
            branches: vec![ahead],
            ..Report::default()
        };

        let out = render(&report, false);
        assert!(out.contains("unpushed-commits"), "was: {out}");
        assert!(!out.contains("(behind)"), "ahead is not behind: {out}");
    }

    #[test]
    fn diverged_origin_is_not_reported_as_behind() {
        let mut row = row("feat/alpha", None, None);
        row.origin_tip = Some(CommitId::new("deadbeefdead"));
        row.origin_relation = Some(OriginRelation::Diverged);
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row],
            forge_consulted: true,
            ..Report::default()
        });

        assert!(out.contains("(diverged)"), "was: {out}");
        assert!(!out.contains("(behind)"), "was: {out}");
    }

    #[test]
    fn unresolved_origin_relation_is_not_reported_as_history() {
        let mut row = row("feat/alpha", None, None);
        row.origin_tip = Some(CommitId::new("deadbeefdead"));
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row],
            forge_consulted: true,
            ..Report::default()
        });

        assert!(out.contains("(unresolved)"), "was: {out}");
        assert!(!out.contains("(behind)"), "was: {out}");
        assert!(!out.contains("(diverged)"), "was: {out}");
    }

    #[test]
    fn an_unresolved_origin_relation_records_its_branch() {
        let mut report = Report::default();
        let branch = BranchName::new("feat/alpha");
        let relation = record_origin_relation(
            &mut report,
            &branch,
            Err(JjError::Revision {
                revision: "0000000000000000000000000000000000000000".to_owned(),
                detail: "missing".to_owned(),
            }),
        );

        assert_eq!(relation, None);
        assert!(
            report.problems.iter().any(|problem| {
                problem.contains("cannot tell how feat/alpha relates to origin")
            })
        );
    }

    #[test]
    fn a_branch_with_no_origin_counterpart_is_shown_as_unpushed() {
        let out = render_verbose(&Report {
            repo: "a".to_owned(),
            branches: vec![row("feat/alpha", None, None)],
            forge_consulted: true,
            ..Report::default()
        });
        assert!(out.contains("unpushed"), "was: {out}");
        assert!(!out.contains("unpushed-commits"), "was: {out}");
    }

    #[test]
    fn a_report_that_did_not_consult_the_forge_says_so() {
        let report = Report {
            repo: "a-repo".to_owned(),
            forge_consulted: false,
            ..Report::default()
        };
        assert!(render(&report, true).contains("not checked"));
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
