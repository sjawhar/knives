//! `knives status`: per-branch state and all four detectors against a live repo.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use crate::bind::Fork;
use crate::cli::Exit;
use crate::config::{Registry, RepoEntry, Role};
use crate::detect::{
    BookmarkTips, Finding, FindingKind, LandedVerdict, Subject, classify_landed, divergent_changes,
    double_checkout,
};
use crate::forge::{
    ChecksSummary, Forge, PullDetails, PullIndex, PullRequest, PullSummary, index_pulls,
};
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, ReleaseScheme, RepoName, is_release_name,
    pull_number_from_bookmark, short_id,
};
use crate::jj::{JjError, Repo, probe_landed, repo_config_path, repo_immutable_heads};
use crate::ledger::{Entry as Notch, Ledger};
use crate::store::Store;

mod claims;
mod dependencies;
mod merged;
mod overlap;
pub mod phases;
mod releases;
pub mod render;
mod rows;

use claims::{ClaimFoldInput, fold_claims, notches_from_ledger, repo_notches};
use dependencies::{DependencyInput, add_dependency_findings};
use overlap::{CarriedFindingInput, add_branch_overlap_findings, carried_findings};
use releases::{ReleaseInput, add_releases};
#[derive(Debug, Clone)]
pub struct BranchRow {
    pub name: BranchName,
    pub state: BranchState,
    /// Short (12-char) commit id; absent when the bookmark is divergent.
    pub tip: Option<String>,
    /// Present only when the local branch does not cleanly match origin.
    pub push: Option<PushRelation>,
    pub pr: Option<PullCell>,
    pub review: Option<String>,
    pub checks: Option<String>,
    pub landed: Option<LandedVerdict>,
    pub flags: Vec<String>,
    pub claim: Option<ClaimCell>,
    pub last_seen: Option<String>,
    pub seen: Option<SeenWindow>,
    pub workspace: Option<String>,
    pub notch: Option<LastNotch>,
}

impl serde::Serialize for BranchRow {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(serde::Serialize)]
        struct SerializedBranchRow<'a> {
            name: &'a BranchName,
            state: &'a BranchState,
            #[serde(skip_serializing_if = "Option::is_none")]
            tip: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            push: Option<&'a PushRelation>,
            #[serde(skip_serializing_if = "Option::is_none")]
            origin_tip: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pr: Option<&'a PullCell>,
            #[serde(skip_serializing_if = "Option::is_none")]
            review: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            checks: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            landed: Option<&'a LandedVerdict>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            flags: &'a Vec<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            claim: Option<&'a ClaimCell>,
            #[serde(skip_serializing_if = "Option::is_none")]
            last_seen: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            seen: Option<&'a SeenWindow>,
            #[serde(skip_serializing_if = "Option::is_none")]
            workspace: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            notch: Option<&'a LastNotch>,
        }

        serde::Serialize::serialize(
            &SerializedBranchRow {
                name: &self.name,
                state: &self.state,
                tip: self.tip.as_ref(),
                push: self.push.as_ref(),
                origin_tip: self.push.as_ref().and_then(PushRelation::origin_tip),
                pr: self.pr.as_ref(),
                review: self.review.as_ref(),
                checks: self.checks.as_ref(),
                landed: self.landed.as_ref(),
                flags: &self.flags,
                claim: self.claim.as_ref(),
                last_seen: self.last_seen.as_ref(),
                seen: self.seen.as_ref(),
                workspace: self.workspace.as_ref(),
                notch: self.notch.as_ref(),
            },
            serializer,
        )
    }
}

impl BranchRow {
    const fn bare(name: BranchName) -> Self {
        Self {
            name,
            state: BranchState::Unknown,
            tip: None,
            push: None,
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
    /// When the newest review or comment landed, from the live fact. The
    /// review column carries the forge's decision, and a comment-only review
    /// or a maintainer's question leaves that decision empty; this is how a
    /// reader sees that the pull request moved since they last looked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_at: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRelation {
    Unpushed,
    UnpushedCommits,
    Behind(String),
    Diverged(String),
    Unresolved(String),
}

impl PushRelation {
    const fn label(&self) -> &'static str {
        match self {
            Self::Unpushed => "unpushed",
            Self::UnpushedCommits => "unpushed-commits",
            Self::Behind(_) => "behind",
            Self::Diverged(_) => "diverged",
            Self::Unresolved(_) => "unresolved",
        }
    }

    fn origin_tip(&self) -> Option<&str> {
        match self {
            Self::Behind(origin) | Self::Diverged(origin) | Self::Unresolved(origin) => {
                Some(origin)
            }
            Self::Unpushed | Self::UnpushedCommits => None,
        }
    }
}

impl serde::Serialize for PushRelation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.label())
    }
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

/// Every finding of one kind, in detector order.
#[derive(Debug, serde::Serialize)]
pub struct FindingGroup {
    pub kind: FindingKind,
    pub items: Vec<GroupedFinding>,
}

/// One subject and the one-line fact about it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GroupedFinding {
    pub subject: String,
    pub detail: String,
}

impl FindingGroup {
    pub fn subjects(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|item| item.subject.as_str())
    }
}

/// The most relevant ledger entry for one status row and the entries it masks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastNotch {
    pub ts: String,
    pub kind: crate::ledger::Kind,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    /// The subject's tip when the entry was written. A past-tense entry stays
    /// true at its anchor and may be stale at today's tip: a "NO carried commit"
    /// note measured against one release read as current four days later, when
    /// the branch had since been carried. Short (12 characters), for the row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
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
            anchor: entry
                .anchor
                .as_deref()
                .map(|anchor| short_id(anchor).to_owned()),
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

/// The repository's own jj config stating an `immutable_heads()` other than this
/// entry's. Absence is not reported: `knives start` writes the entry's rule, and a
/// rule nobody stated is nobody's decision to disagree with. A rule knives itself
/// wrote earlier, now stale because the entry changed, is reported too — `start`
/// is what refreshes it, and the detail says so.
fn immutable_heads_finding(entry: &RepoEntry, path: &Path) -> Result<Option<Finding>, JjError> {
    let rule = entry.immutable_heads();
    let Some(stated) = repo_immutable_heads(path)? else {
        return Ok(None);
    };
    if stated.rule == rule {
        return Ok(None);
    }
    let detail = if stated.written_by_knives {
        format!(
            "repo config states immutable_heads() = `{}`, written by an earlier `knives start`; \
             this entry's rule is `{rule}`, which the next `knives start` writes",
            stated.rule
        )
    } else {
        format!(
            "repo config states immutable_heads() = `{}`; a managed fork runs under `{rule}`, \
             which `knives start` writes where none is stated",
            stated.rule
        )
    };
    Ok(Some(Finding::new(
        FindingKind::ImmutableHeadsRule,
        Subject::File(repo_config_path(path)?.display().to_string()),
        detail,
    )))
}

/// A jj that cannot answer costs one problem line, not the repository's report.
fn add_immutable_heads_finding(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    entry: &RepoEntry,
    path: &Path,
) {
    match immutable_heads_finding(entry, path) {
        Ok(finding) => findings.extend(finding),
        Err(error) => report
            .problems
            .push(format!("immutable_heads() rule not read: {error}")),
    }
}

struct FoldOutput<'a> {
    report: &'a mut Report,
    findings: &'a mut Vec<Finding>,
    timings: &'a mut Timings,
}

struct FinalStatusInput<'a, 'snapshot> {
    unjudged: &'a [String],
    snapshot: Option<&'a crate::snapshot::CompletedSnapshot<'snapshot>>,
    landed: Option<BTreeMap<String, LandedVerdict>>,
}

/// Inputs that turn completed phases into the report's visible rows and findings.
struct PostPhaseInput<'a> {
    fork: &'a Fork<'a>,
    repo: &'a Repo,
    store: &'a Store,
    options: &'a Options<'a>,
    tips: &'a BookmarkTips,
    notches: &'a [Notch],
    /// Every divergent local bookmark with every commit it names: rows that
    /// have no single tip.
    divergent_branches: &'a BTreeMap<BranchName, Vec<CommitId>>,
    probe_ran: bool,
    trunk_commit: Option<&'a CommitId>,
}

/// A `stacked-history` finding for every branch with an open pull request whose
/// history past the trunk carries merges: that pull request asks its reviewer
/// to take everything those merges carried. Branches without a pull request are
/// the release plan's concern, where the same detector runs on the members.
fn stacked_pull_findings(
    report: &Report,
    context: crate::release_model::StackedHistoryContext<'_>,
    tips: &BookmarkTips,
) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();
    for row in &report.branches {
        let Some(pull) = row.pr.as_ref() else {
            continue;
        };
        if !pull.state.eq_ignore_ascii_case("OPEN") {
            continue;
        }
        let Some(tip) = tips.get(&BookmarkRef::Local(row.name.clone())) else {
            continue;
        };
        findings.extend(crate::release_model::stacked_history(
            context,
            row.name.as_str(),
            tip,
        )?);
    }
    Ok(findings)
}

/// What the rows say once the forge and the graph are both in hand: landed
/// verdicts the forge can settle, branches another reference carries, and pull
/// requests whose history carries a release merge.
fn settle_rows_after_phases(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    input: &PostPhaseInput<'_>,
    index: &PullIndex,
) -> anyhow::Result<()> {
    let entry = input.fork.entry;
    merged::settle_merged_landed(
        report,
        merged::MergedLandedInput {
            repo: input.repo,
            index,
            tips: input.tips,
            divergent_tips: input.divergent_branches,
            trunk_tip: input.trunk_commit,
        },
    )?;
    let scheme = entry.release_scheme();
    findings.extend(carried_findings(CarriedFindingInput {
        report,
        repo: input.repo,
        tips: input.tips,
        trunk: entry.trunk(),
        scheme: &scheme,
        publish_remote: entry.publish_remote(),
    })?);
    let trunks = crate::release_model::trunk_positions(input.repo, entry)?;
    if !trunks.is_empty() {
        let releases = crate::release_model::release_refs_by_commit(
            input.tips,
            &scheme,
            entry.publish_remote(),
        );
        findings.extend(stacked_pull_findings(
            report,
            crate::release_model::StackedHistoryContext {
                repo: input.repo,
                trunks: &trunks,
                releases: &releases,
            },
            input.tips,
        )?);
    }
    Ok(())
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
fn persist_landed(
    report: &mut Report,
    snapshot: Option<&crate::snapshot::CompletedSnapshot<'_>>,
    landed: Option<BTreeMap<String, LandedVerdict>>,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    if let Err(note) = snapshot.persist(landed) {
        report.notes.push(note.to_string());
    }
}

fn set_forge_status(
    report: &mut Report,
    snapshot: Option<&crate::snapshot::CompletedSnapshot<'_>>,
    timings: &Timings,
) -> anyhow::Result<()> {
    report.forge = ForgeStatus {
        consulted: snapshot.is_some(),
        elapsed_ms: u64::try_from(timings.forge.as_millis())
            .map_err(|_| anyhow::anyhow!("forge phase elapsed time cannot fit in milliseconds"))?,
    };
    Ok(())
}

fn finalize_status(
    report: &mut Report,
    findings: &mut Vec<Finding>,
    input: FinalStatusInput<'_, '_>,
) {
    // The forge may have settled a replay the probe could not judge (a merged
    // pull request whose head the trunk now has); a row that reads `in-trunk`
    // is not a branch nobody could judge.
    let still_unjudged: Vec<String> = input
        .unjudged
        .iter()
        .filter(|name| {
            report.branches.iter().any(|row| {
                row.name.as_str() == name.as_str()
                    && matches!(row.landed, Some(LandedVerdict::Unjudged))
            })
        })
        .cloned()
        .collect();
    report.problems.extend(unjudged_note(&still_unjudged));
    if let Some(snapshot) = input.snapshot {
        add_pull_state_findings(report, findings, snapshot);
    }
    persist_landed(report, input.snapshot, input.landed);
}

fn append_divergent_rows(
    input: &rows::DivergentInput<'_, '_>,
    report: &mut Report,
    findings: &mut Vec<Finding>,
) -> std::time::Duration {
    let phase = std::time::Instant::now();
    let divergent = rows::divergent_rows(input, report, findings);
    report.branches.extend(divergent);
    report
        .branches
        .sort_by(|left, right| left.name.cmp(&right.name));
    phase.elapsed()
}

/// Fold completed phases into rows, findings, timings, and cache persistence.
fn fold_phase_outcome(
    output: FoldOutput<'_>,
    input: &PostPhaseInput<'_>,
    probe_inputs: Vec<phases::ProbeInput>,
    phases: &mut phases::StatusPhases<'_>,
) -> anyhow::Result<()> {
    let FoldOutput {
        report,
        findings,
        timings,
    } = output;
    let snapshot = phases.forge.snapshot.as_ref();
    let empty_index = PullIndex::default();
    let index = snapshot.map_or(&empty_index, crate::snapshot::CompletedSnapshot::index);
    report
        .problems
        .extend(std::mem::take(&mut phases.forge.problems));
    timings.forge = phases.forge.duration;
    timings.probes = phases.probe.duration;

    let name = &input.fork.name;
    let entry = input.fork.entry;
    let phase = std::time::Instant::now();
    let origin_phase = phases::origin_phase(
        &input.fork.checkout.path,
        &probe_inputs,
        input.options.workers,
    );
    let unjudged = rows::branch_rows(
        rows::RowInput {
            name,
            store: input.store,
            probe_inputs,
            verdicts: std::mem::take(&mut phases.probe.verdicts),
            origin_relations: origin_phase.relations,
            index,
            snapshot,
            notches: input.notches,
            expected_base: entry.default_base(),
        },
        report,
        findings,
    )?;
    timings.origin_relations = phase.elapsed();

    let divergent_names: Vec<BranchName> = input.divergent_branches.keys().cloned().collect();
    timings.divergent_rows = append_divergent_rows(
        &rows::DivergentInput {
            branches: &divergent_names,
            tips: input.tips,
            name,
            store: input.store,
            snapshot,
            index,
            notches: input.notches,
            expected_base: entry.default_base(),
        },
        report,
        findings,
    );
    let phase = std::time::Instant::now();
    settle_rows_after_phases(report, findings, input, index)?;
    timings.carried_findings = phase.elapsed();

    timings.touching =
        add_branch_overlap_findings(report, findings, input.fork, input.options.workers);

    let phase = std::time::Instant::now();
    let seen = crate::seen::load();
    fold_claims(
        report,
        findings,
        ClaimFoldInput {
            repo: input.repo,
            name,
            store: input.store,
            seen: &seen,
            tips: input.tips,
        },
    )?;
    timings.claims = phase.elapsed();

    let phase = std::time::Instant::now();
    let landed = input
        .probe_ran
        .then(|| std::mem::take(&mut phases.probe.landed));
    add_dependency_findings(DependencyInput {
        report,
        findings,
        name,
        path: &input.fork.checkout.path,
        store: input.store,
        options: input.options,
        snapshot,
        timings,
    });
    finalize_status(
        report,
        findings,
        FinalStatusInput {
            unjudged: &unjudged,
            snapshot,
            landed,
        },
    );
    set_forge_status(report, snapshot, timings)?;
    timings.report = phase.elapsed();
    Ok(())
}

/// The forge snapshot, or `None` after noting why the forge could not answer.
fn open_forge_or_note<'a>(
    report: &mut Report,
    fork: &'a Fork<'a>,
    options: &'a Options<'a>,
) -> Option<crate::snapshot::Opened<'a>> {
    match phases::open_forge_snapshot(
        options.forge,
        fork.entry,
        &fork.checkout.path,
        options.cache,
    ) {
        Ok(opened) => opened,
        Err(error) => {
            report
                .problems
                .push(format!("pull request state unavailable: {error}"));
            None
        }
    }
}

/// The report, and where the run spent its time.
///
/// One function rather than two paths, so a measured run and an unmeasured one
/// cannot drift: `gather` is this with the measurement dropped.
pub fn gather_timed(
    fork: &Fork<'_>,
    store: &Store,
    options: &Options<'_>,
) -> anyhow::Result<(Report, Timings)> {
    let name = &fork.name;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let started = std::time::Instant::now();
    let mut timings = Timings::default();
    let phase = std::time::Instant::now();
    let repo = Repo::open(path)?;
    let mut report = Report {
        repo: name.to_string(),
        trunk: entry.trunk().to_owned(),
        notes: fork.remote_notes(),
        ..Report::default()
    };
    let mut findings = Vec::new();
    let tips = repo.bookmark_tips()?;
    timings.repository = phase.elapsed();
    let phase = std::time::Instant::now();
    add_releases(
        &mut report,
        &mut findings,
        ReleaseInput {
            repo: &repo,
            tips: &tips,
            entry,
            path,
        },
    )?;
    timings.releases = phase.elapsed();

    let phase = std::time::Instant::now();
    let (branches, fetched_heads) =
        rows::maintained_branches(&tips, entry.trunk(), &entry.release_scheme());
    let divergent_branches = rows::divergent_branches(&repo, entry)?;
    let probe_inputs = phases::probe_inputs(branches, &tips);
    let mut all_branches: Vec<BranchName> = probe_inputs
        .iter()
        .map(|input| input.branch.clone())
        .collect();
    all_branches.extend(divergent_branches.keys().cloned());
    let declared = phases::declared_numbers(name, &all_branches, store);
    let notches = notches_from_ledger(options.ledger, &mut report);
    report.repo_notches = repo_notches(&notches);
    note_fetched_heads(&mut report, fetched_heads);
    add_immutable_heads_finding(&mut report, &mut findings, entry, path);
    timings.setup = phase.elapsed();

    let forge_started = std::time::Instant::now();
    let opened = open_forge_or_note(&mut report, fork, options);
    let opened_ref = opened.as_ref();
    let trunk_commit = repo.resolve_commit(&entry.upstream_trunk()).ok();
    let probe_ran = options.probe && trunk_commit.is_some();
    let (mut phases, health) = std::thread::scope(|scope| {
        let health = scope.spawn(|| phases::repository_health(path, &tips, entry.publish_remote()));
        let phases = phases::run_status_phases(phases::StatusPhaseInput {
            entry,
            path,
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
        FoldOutput {
            report: &mut report,
            findings: &mut findings,
            timings: &mut timings,
        },
        &PostPhaseInput {
            fork,
            repo: &repo,
            store,
            options,
            tips: &tips,
            notches: &notches,
            divergent_branches: &divergent_branches,
            probe_ran,
            trunk_commit: trunk_commit.as_ref(),
        },
        probe_inputs,
        &mut phases,
    )?;
    report.findings = group_findings(findings);
    timings.total = started.elapsed();
    Ok((report, timings))
}

pub fn gather(fork: &Fork<'_>, store: &Store, options: &Options<'_>) -> anyhow::Result<Report> {
    gather_timed(fork, store, options).map(|(report, _)| report)
}

/// One grouped subject per raw finding. Relationship findings include the
/// counterpart because the lone branch or path does not identify the action.
fn grouped_subject(finding: &Finding) -> String {
    let subject = finding.subject.short();
    let counterpart = match finding.kind {
        FindingKind::BranchOverlap => finding
            .detail
            .strip_prefix(&format!("{subject} is touched by ")),
        FindingKind::CarriedElsewhere => finding
            .detail
            .strip_prefix(&format!("{subject}'s tip is also reachable from ")),
        _ => None,
    };
    counterpart.map_or_else(
        || subject.clone(),
        |counterpart| format!("{subject}: {counterpart}"),
    )
}

/// Folds raw findings once, after every detector has reported, preserving detector order.
///
/// Every finding keeps its subject and its detail. The one-line-per-kind text
/// view shows the first few subjects; `--verbose` and the machine output carry
/// all of them with their detail, because a subject alone (`#11`) does not say
/// which workflow never ran or which release a stacked branch carries, and a
/// reader who asked for detail on the ninth finding was not asking for the
/// first eight.
fn group_findings(findings: Vec<Finding>) -> Vec<FindingGroup> {
    let mut groups: Vec<FindingGroup> = Vec::new();
    for finding in findings {
        let subject = grouped_subject(&finding);
        let item = GroupedFinding {
            subject,
            detail: finding.detail,
        };
        if let Some(group) = groups.iter_mut().find(|group| group.kind == finding.kind) {
            group.items.push(item);
        } else {
            groups.push(FindingGroup {
                kind: finding.kind,
                items: vec![item],
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
    use super::{claims::claim_last_seen, releases::releases_to_scan};

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
        assert!(
            !json.contains("null"),
            "absent values are skipped, never null: {json}"
        );
        assert!(
            !json.contains("\"claims\""),
            "the standalone claims section is dead: {json}"
        );
    }

    #[test]
    fn findings_group_by_kind_in_first_seen_order_and_keep_every_subject() {
        let mut findings = vec![Finding::new(
            FindingKind::WrongBase,
            Subject::PullRequest(1),
            "first",
        )];
        findings.extend((0..30).map(|number| {
            Finding::new(
                FindingKind::Divergence,
                Subject::Branch(BranchName::new(format!("feat/{number}"))),
                format!("feat/{number} is on two commits"),
            )
        }));

        let groups = group_findings(findings);

        // Every subject and its detail survive grouping: the one-line text view
        // truncates, the machine output and --verbose do not.
        assert_eq!(groups.len(), 2, "was: {groups:?}");
        assert_eq!(groups[0].kind, FindingKind::WrongBase);
        assert_eq!(groups[1].kind, FindingKind::Divergence);
        assert_eq!(groups[1].items.len(), 30);
        assert_eq!(groups[1].items[0].subject, "feat/0");
        assert_eq!(groups[1].items[29].detail, "feat/29 is on two commits");
        let text = render::render(
            &Report {
                findings: groups,
                ..Report::default()
            },
            false,
        );
        assert!(
            text.contains("feat/7, and 22 more") && !text.contains("feat/8"),
            "the one-line view caps at eight subjects: {text}"
        );
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
    fn activity_errors_keep_sidecar_observations_and_mark_unsighted_claims_within_window() {
        let claim = crate::store::Claim {
            repo: "demo".to_owned(),
            branch: "feat/alpha".to_owned(),
            owner: "session".to_owned(),
            kind: crate::store::OwnerKind::HarnessSession,
            why: "status model".to_owned(),
            started: "2026-08-01T00:00:00Z".to_owned(),
            files: Vec::new(),
        };

        assert_eq!(
            claim_last_seen(&claim, None, &crate::seen::Seen::default()),
            crate::seen::LastSeen::NoneWithinWindow
        );

        let seen = crate::seen::Seen {
            owners: std::collections::BTreeMap::from([(
                crate::store::OwnerKind::HarnessSession,
                std::collections::BTreeMap::from([(
                    "session".to_owned(),
                    jiff::Timestamp::now().to_string(),
                )]),
            )]),
            ..crate::seen::Seen::default()
        };
        assert!(matches!(
            claim_last_seen(&claim, None, &seen),
            crate::seen::LastSeen::At(_)
        ));
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
            parents: Vec::new(),
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
        let names: Vec<String> = chosen
            .iter()
            .map(|(reference, _)| reference.to_string())
            .collect();

        assert_eq!(
            names,
            vec!["release/2026-07-28", "release/2026-07-29@origin"]
        );
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
        let names: Vec<String> = chosen
            .iter()
            .map(|(reference, _)| reference.to_string())
            .collect();

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
            upstream: String::new(),
            origin: String::new(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        };

        let fork = Fork::at("demo", &entry, Path::new(""));
        let _ = add_branch_overlap_findings(&mut report, &mut Vec::new(), &fork, 1);

        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no single tip"))
        );
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
            upstream: String::new(),
            origin: String::new(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
            workspaces: None,
        };

        let fork = Fork::at("demo", &entry, scratch.path());
        let _ = add_branch_overlap_findings(&mut report, &mut Vec::new(), &fork, 1);

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
                items: vec![GroupedFinding {
                    subject: "feat/a".to_owned(),
                    detail: "feat/a is on two commits".to_owned(),
                }],
            }],
            ..Report::default()
        };
        assert_eq!(exit_for(&dirty), Exit::Findings);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one fixture exercising every row cell at once is the point of the scale snapshot"
    )]
    fn swe_scale_fixture() -> Report {
        let branches = (0..14)
            .map(|index| {
                let mut row = BranchRow::bare(BranchName::new(format!("feat/scale-{index:02}")));
                row.state = match index % 4 {
                    0 => BranchState::ChecksFailing,
                    1 => BranchState::Approved,
                    2 => BranchState::AwaitingReview,
                    _ => BranchState::NoPr,
                };
                if index == 0 {
                    row.pr = Some(PullCell {
                        number: 4891,
                        state: "open".to_owned(),
                        draft: false,
                        stated: None,
                        activity_at: Some("2026-08-30T09:15:00Z".to_owned()),
                        prior: Vec::new(),
                    });
                    row.review = Some("changes-requested".to_owned());
                    row.checks = Some("failing".to_owned());
                    row.flags.push("review-stale".to_owned());
                }
                if index == 1 {
                    row.push = Some(PushRelation::Behind("f0e1d2c3b4a5".to_owned()));
                }
                if index == 2 {
                    row.seen = Some(SeenWindow::NoneWithinWindow);
                    row.workspace = Some("scale-02".to_owned());
                }
                if index == 3 {
                    row.notch = Some(LastNotch {
                        ts: "2026-08-30T09:15:00Z".to_owned(),
                        kind: crate::ledger::Kind::Note,
                        text: "Release triage recorded after the final merge queue drain.".to_owned(),
                        disposition: Some("decided".to_owned()),
                        anchor: Some("0f1e2d3c4b5a".to_owned()),
                        count: 4,
                    });
                }
                if index < 6 {
                    row.claim = Some(ClaimCell {
                        id: format!("session-{index:012x}"),
                        kind: crate::store::OwnerKind::HarnessSession,
                        since: "2026-08-29T12:00:00Z".to_owned(),
                        why: format!(
                            "Migrate the status integration assertions and verify the release report for branch {index}."
                        ),
                    });
                }
                row
            })
            .collect();
        let finding = |kind, count, prefix: &str| FindingGroup {
            kind,
            items: (0..count)
                .map(|index| GroupedFinding {
                    subject: format!("{prefix}-{index:02}"),
                    detail: format!("{prefix}-{index:02} needs attention"),
                })
                .collect(),
        };

        Report {
            repo: "swe-scale".to_owned(),
            trunk: "main".to_owned(),
            newest_release: Some("release/2026-08-29".to_owned()),
            forge: ForgeStatus {
                consulted: true,
                elapsed_ms: 347,
            },
            problems: vec![
                "pull request state unavailable: the forge rejected one facts batch".to_owned(),
                "workspace activity unavailable: the local sidecar could not be read".to_owned(),
            ],
            branches,
            findings: vec![
                finding(FindingKind::ChecksFailing, 6, "checks"),
                finding(FindingKind::StaleReview, 6, "review"),
                finding(FindingKind::WrongBase, 6, "base"),
                finding(FindingKind::BranchOverlap, 6, "path"),
                finding(FindingKind::ClaimOverlap, 5, "claim"),
                finding(FindingKind::CarriedElsewhere, 5, "carrier"),
            ],
            releases: vec![
                "release/2026-08-22".to_owned(),
                "release/2026-08-29".to_owned(),
            ],
            repo_notches: Some(RepoNotches {
                count: 4,
                last: LastNotch {
                    ts: "2026-08-30T09:15:00Z".to_owned(),
                    kind: crate::ledger::Kind::Note,
                    text: "Release triage recorded after the final merge queue drain.".to_owned(),
                    disposition: Some("decided".to_owned()),
                    anchor: None,
                    count: 4,
                },
            }),
            other_workspaces: vec![
                "legacy-release".to_owned(),
                "manual-repro".to_owned(),
                "untracked-experiment".to_owned(),
            ],
            notes: Vec::new(),
        }
    }

    #[test]
    fn a_swe_scale_report_encodes_within_the_map_budget() {
        // 14 branches (6 claimed with real-length whys), 34 findings across 6 kinds,
        // 2 problems, 2 releases, repo notches, and 3 other workspaces: the report
        // shape that rendered 393 TOON lines before the status map.
        let report = swe_scale_fixture();
        assert_eq!(report.branches.len(), 14);
        assert_eq!(
            report
                .branches
                .iter()
                .filter(|row| row.claim.is_some())
                .count(),
            6
        );
        assert_eq!(report.findings.len(), 6);
        assert_eq!(
            report
                .findings
                .iter()
                .map(|finding| finding.items.len())
                .sum::<usize>(),
            34
        );
        assert_eq!(report.problems.len(), 2);
        assert_eq!(report.releases.len(), 2);
        assert!(report.repo_notches.is_some());
        assert_eq!(report.other_workspaces.len(), 3);
        assert!(report.branches.iter().any(|row| row.pr.is_some()));
        assert!(report.branches.iter().any(|row| row.review.is_some()));
        assert!(report.branches.iter().any(|row| row.checks.is_some()));
        assert!(report.branches.iter().any(|row| !row.flags.is_empty()));
        assert!(report.branches.iter().any(|row| row.push.is_some()));
        assert!(report.branches.iter().any(|row| {
            row.push
                .as_ref()
                .and_then(PushRelation::origin_tip)
                .is_some()
        }));
        assert!(report.branches.iter().any(|row| row.seen.is_some()));
        assert!(report.branches.iter().any(|row| row.workspace.is_some()));
        assert!(report.branches.iter().any(|row| row.notch.is_some()));

        // Findings are one tabular row each, so their share of the encoding is
        // exactly the findings and nothing more; everything else stays a map.
        let toon = toon_format::encode_default(&report).expect("encode");
        let lines = toon.lines().count();
        let groups = report.findings.len();
        let findings: usize = report.findings.iter().map(|group| group.items.len()).sum();
        let findings_lines = groups * 2 + findings;
        assert!(
            lines - findings_lines <= 100,
            "the map regressed to a dump: {lines} lines, {findings_lines} of them findings\n{toon}"
        );
        assert!(
            toon.contains("items[6]{subject,detail}:"),
            "findings must encode as one row per subject: {toon}"
        );
    }

    /// Guards the pinned serde-order-equals-TOON-presentation-order invariant:
    /// an unanswered forge decode must lead the branch section.
    #[test]
    fn serde_order_keeps_a_forge_decode_failure_before_branches() {
        let report = Report {
            repo: "a".into(),
            trunk: "main".into(),
            problems: vec![
                "pull request state unavailable: could not read the forge's reply: …".into(),
            ],
            branches: vec![BranchRow::bare(BranchName::new("feat/alpha"))],
            ..Report::default()
        };

        let toon = toon_format::encode_default(&report).expect("encode");
        let head: String = toon.lines().take(6).collect::<Vec<_>>().join("\n");
        let problem_at = toon
            .find("pull request state unavailable")
            .expect("forge decode failure is rendered");
        let branches_at = toon
            .find("branches[1]")
            .expect("branch section is rendered");
        assert!(
            head.contains("pull request state unavailable"),
            "was:\n{toon}"
        );
        assert!(problem_at < branches_at, "problems must lead: {toon}");
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }
}
