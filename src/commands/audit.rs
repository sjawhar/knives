//! `knives audit`: reconcile the fork estate against live refs and recorded facts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::bind::Fork;
use crate::cli::Exit;
use crate::commands::pushed::{self, ReconcileInput, Row};
use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, Finding, FindingKind, Subject};
use crate::forge::{Forge, PullRequest};
use crate::ids::{
    BookmarkRef, BranchName, BranchTarget, CommitId, UpstreamName, is_release_name, short_id,
};
use crate::jj::{self, Repo};
use crate::ledger::Entry;
use crate::release_model::carried_from_tips;
use crate::snapshot::{self, SnapshotConfig};
use crate::store::Store;

mod branches;

pub use branches::{BranchFacts, CheckCounts, PullSnapshot, Template};
use branches::{
    LocalFacts, RowInput, add_branch_facts, attach_pulls, read_template, render_branch,
};

const ORPHAN_REVSET: &str = r#"heads(all()) ~ ::(bookmarks() | remote_bookmarks() | tags()) ~ working_copies() ~ (empty() & description(exact:""))"#;

/// Every audit finding, note, and unanswered check for one repository, with
/// one row of facts per maintained branch.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    /// Upstream's pull-request template, read once per run; `None` when the
    /// trunk carries none or no forge was asked.
    pub template: Option<Template>,
    /// One row per maintained branch: what is observed, never what to do.
    pub branches: Vec<BranchFacts>,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

impl Report {
    /// An empty report for `repo`, before anything is observed.
    pub fn new(repo: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            template: None,
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        }
    }
}

/// Dependencies shared by the read-only estate checks.
pub struct AuditInput<'a> {
    pub fork: &'a Fork<'a>,
    pub store: &'a Store,
    pub forge: Option<&'a dyn Forge>,
    pub cache_root: Option<&'a Path>,
    /// Threads for the per-branch diff scans.
    pub workers: usize,
}

impl std::fmt::Debug for AuditInput<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditInput")
            .field("fork", self.fork)
            .field("forge", &self.forge.is_some())
            .field("cache_root", &self.cache_root)
            .field("workers", &self.workers)
            .finish()
    }
}

/// Gather the estate facts without writing a repository, remote, store, or ledger.
pub fn gather(input: &AuditInput<'_>) -> Report {
    let fork = input.fork;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let mut report = Report::new(fork.name.to_string());
    let opened = match Repo::open(path) {
        Ok(opened) => opened,
        Err(error) => {
            report
                .problems
                .push(format!("could not open {}: {error}", path.display()));
            return report;
        }
    };
    let tips = match opened.bookmark_tips() {
        Ok(tips) => tips,
        Err(error) => {
            report
                .problems
                .push(format!("could not read local bookmarks: {error}"));
            return report;
        }
    };
    let conflicted = match opened.conflicted_bookmarks() {
        Ok(conflicted) => conflicted,
        Err(error) => {
            report
                .problems
                .push(format!("could not read conflicted bookmarks: {error}"));
            return report;
        }
    };
    add_unconfigured_remote_refs(&mut report, &fork.checkout.remotes, &tips, &conflicted);
    let scheme = entry.release_scheme();
    let carried = carried_from_tips(&tips, entry.trunk(), &scheme);
    let local = pushed::local_tips(tips);
    let live = match pushed::live_refs(entry) {
        Ok(live) => live,
        Err(error) => {
            report
                .problems
                .push(format!("could not read live push refs: {error}"));
            return report;
        }
    };
    let requested: Vec<BranchName> = local.keys().cloned().collect();
    let tracked = tracked(input.store, &fork.upstream, &requested);
    let rows = pushed::reconcile(&ReconcileInput {
        tips_local: &local,
        origin_refs: live.origin(),
        release_refs: live.release(),
        scheme: &scheme,
        tracked: &tracked,
        requested: &requested,
    });
    report.findings.extend(remote_drifts(&rows));
    report.findings.extend(zombie_branches(&ZombieInput {
        entry,
        store: input.store,
        repo: &fork.upstream,
        local: &local,
        live: &live,
        scheme: &scheme,
    }));
    add_release_drifts(
        &mut report,
        &ReleaseDriftScan {
            local: &local,
            published: live.release(),
            schemes: &release_schemes(entry),
            publish_remote: entry.publish_remote(),
            entries: input.store.ledger_entries(&fork.upstream),
            is_metadata_rewrite: &|recorded, current| opened.is_metadata_rewrite(recorded, current),
        },
    );
    add_misplaced_origin_release_refs(&mut report, live.origin(), &scheme, entry.publish_remote());
    add_unfit_branches(&mut report, &opened, entry, &carried);
    add_orphan_commits(&mut report, &opened, path);
    let template = template_for_pulls(&mut report, input);
    add_branch_facts(
        &mut report,
        input,
        &opened,
        &RowInput {
            origin_refs: live.origin(),
            carried: &carried,
            conflicted: &conflicted,
        },
    );
    add_open_pull_head_checks(
        &mut report,
        input,
        &LocalFacts {
            local: &local,
            origin_refs: live.origin(),
            tracked: &tracked,
            template: template.as_ref(),
        },
    );
    report.template = template;
    report
}

/// Upstream's pull-request template, read only when a forge will answer the
/// bodies it is held against; an unreadable template is a problem line.
fn template_for_pulls(report: &mut Report, input: &AuditInput<'_>) -> Option<Template> {
    input.forge?;
    let entry = input.fork.entry;
    read_template(&input.fork.checkout.path, entry).unwrap_or_else(|error| {
        report.problems.push(format!(
            "could not read the pull-request template at {}: {error}",
            entry.trunk_ref()
        ));
        None
    })
}

fn add_unconfigured_remote_refs(
    report: &mut Report,
    configured: &BTreeMap<String, String>,
    tips: &BookmarkTips,
    conflicted: &[(BookmarkRef, Vec<CommitId>)],
) {
    let unconfigured: BTreeSet<&str> = tips
        .keys()
        .chain(conflicted.iter().map(|(reference, _)| reference))
        .filter_map(|reference| match reference {
            BookmarkRef::Local(_) => None,
            BookmarkRef::Remote { remote, .. } => Some(remote.as_str()),
        })
        .filter(|remote| *remote != "git" && !configured.contains_key(*remote))
        .collect();
    report.findings.extend(
        tips.keys()
            .chain(conflicted.iter().map(|(reference, _)| reference))
            .filter_map(|reference| match reference {
                BookmarkRef::Local(_) => None,
                BookmarkRef::Remote { remote, .. } => unconfigured
                    .contains(remote.as_str())
                    .then(|| {
                        Finding::new(
                            FindingKind::UnconfiguredRemote,
                            Subject::Bookmark(reference.clone()),
                            format!(
                                "remote {remote} is not configured; a fetch will never update this remote-tracking ref"
                            ),
                        )
                    }),
            }),
    );
}

fn tracked(
    store: &Store,
    repo: &UpstreamName,
    branches: &[BranchName],
) -> BTreeMap<BranchName, u64> {
    branches
        .iter()
        .filter_map(|branch| {
            store
                .tracked_pull(&BranchTarget::new(repo.clone(), branch.clone()))
                .map(|number| (branch.clone(), number))
        })
        .collect()
}

fn remote_drifts(rows: &[Row]) -> Vec<Finding> {
    rows.iter()
        .filter(|row| row.verdicts.iter().any(pushed_finding))
        .map(|row| {
            Finding::new(
                FindingKind::RemoteDrift,
                Subject::Branch(BranchName::new(&row.branch)),
                pushed::render_row(row),
            )
        })
        .collect()
}

const fn pushed_finding(verdict: &pushed::Verdict) -> bool {
    !matches!(
        verdict,
        pushed::Verdict::InSync | pushed::Verdict::GoneEverywhere
    )
}

struct ZombieInput<'a> {
    entry: &'a RepoEntry,
    store: &'a Store,
    repo: &'a UpstreamName,
    local: &'a BTreeMap<BranchName, CommitId>,
    live: &'a pushed::LiveRefs,
    scheme: &'a crate::ids::ReleaseScheme,
}

struct OriginZombieInput<'a> {
    refs: &'a BTreeMap<String, CommitId>,
    local: &'a BTreeMap<BranchName, CommitId>,
    claimed: &'a BTreeSet<&'a str>,
    scheme: &'a crate::ids::ReleaseScheme,
    trunk: &'a str,
}

fn zombie_branches(input: &ZombieInput<'_>) -> Vec<Finding> {
    let claimed: BTreeSet<&str> = input
        .store
        .claims(Some(input.repo))
        .into_iter()
        .map(|claim| claim.branch.as_str())
        .collect();
    let mut findings = origin_zombies(&OriginZombieInput {
        refs: input.live.origin(),
        local: input.local,
        claimed: &claimed,
        scheme: input.scheme,
        trunk: input.entry.trunk(),
    });
    if input.entry.remote(Role::Release) != input.entry.remote(Role::Origin) {
        findings.extend(release_zombies(input.live.release(), input.scheme));
    }
    findings
}

fn origin_zombies(input: &OriginZombieInput<'_>) -> Vec<Finding> {
    input
        .refs
        .iter()
        .filter_map(|(reference, commit)| {
            let name = reference.strip_prefix("refs/heads/")?;
            let branch = BranchName::new(name);
            (!input.local.contains_key(&branch)
                && !input.claimed.contains(name)
                && !is_release_name(&branch, input.scheme)
                && name != input.trunk)
                .then(|| {
                    Finding::new(
                        FindingKind::ZombieBranch,
                        Subject::Branch(branch.clone()),
                        format!(
                            "origin has {branch} at {} — no local bookmark or claim",
                            commit.short()
                        ),
                    )
                })
        })
        .collect()
}

fn release_zombies(
    refs: &BTreeMap<String, CommitId>,
    scheme: &crate::ids::ReleaseScheme,
) -> Vec<Finding> {
    refs.iter()
        .filter_map(|(reference, commit)| {
            let name = reference.strip_prefix("refs/heads/")?;
            let branch = BranchName::new(name);
            (!is_release_name(&branch, scheme)).then(|| {
                Finding::new(
                    FindingKind::ZombieBranch,
                    Subject::Branch(branch.clone()),
                    format!(
                        "release has {branch} at {} — not a release ref",
                        commit.short()
                    ),
                )
            })
        })
        .collect()
}

struct ReleaseDriftScan<'a> {
    local: &'a BTreeMap<BranchName, CommitId>,
    published: &'a BTreeMap<String, CommitId>,
    schemes: &'a [crate::ids::ReleaseScheme],
    publish_remote: &'a str,
    entries: &'a [Entry],
    is_metadata_rewrite: &'a MetadataRewrite<'a>,
}

/// A note for each branch that fits no release series of an entry that has
/// named series: every release verb refuses it, so audit says so up front.
fn add_unfit_branches(
    report: &mut Report,
    opened: &Repo,
    entry: &RepoEntry,
    carried: &[(String, CommitId)],
) {
    match crate::release_model::series_members(opened, entry, carried.to_vec()) {
        Ok((_, unfit)) => report.notes.extend(unfit),
        Err(error) => report.problems.push(format!(
            "could not place branches in release series: {error}"
        )),
    }
}

/// Every series' release scheme: each series' cuts are recorded in the same
/// ledger, so drift is checked for all of them, not only the default's.
fn release_schemes(entry: &RepoEntry) -> Vec<crate::ids::ReleaseScheme> {
    entry
        .all_series()
        .iter()
        .map(RepoEntry::release_scheme)
        .collect()
}

struct ReleaseDrift<'a> {
    entries: &'a [Entry],
    reference: BookmarkRef,
    current: &'a CommitId,
    source: DriftSource,
    is_metadata_rewrite: &'a MetadataRewrite<'a>,
}

/// Which copy of a release name a drift check reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DriftSource {
    Local,
    /// The live publish remote, which keeps an earlier write until it is pushed.
    PublishRemote,
}

impl std::fmt::Display for DriftSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Local => "local",
            Self::PublishRemote => "publish remote",
        })
    }
}

/// Whether the second commit is the first one re-signed or metaedited:
/// [`Repo::is_metadata_rewrite`], behind a seam the unit tests can answer.
type MetadataRewrite<'a> = dyn Fn(&str, &str) -> Result<bool, jj::JjError> + 'a;

fn add_release_drifts(report: &mut Report, scan: &ReleaseDriftScan<'_>) {
    let ReleaseDriftScan {
        local,
        published,
        schemes,
        publish_remote,
        entries,
        is_metadata_rewrite,
    } = *scan;
    for (branch, current) in local
        .iter()
        .filter(|(branch, _)| schemes.iter().any(|scheme| is_release_name(branch, scheme)))
    {
        add_release_drift(
            report,
            ReleaseDrift {
                entries,
                reference: BookmarkRef::Local(branch.clone()),
                current,
                source: DriftSource::Local,
                is_metadata_rewrite,
            },
        );
    }
    for (reference, current) in published {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        let branch = BranchName::new(name);
        if !schemes
            .iter()
            .any(|scheme| is_release_name(&branch, scheme))
        {
            continue;
        }
        add_release_drift(
            report,
            ReleaseDrift {
                entries,
                reference: BookmarkRef::Remote {
                    branch,
                    remote: crate::ids::RemoteName::new(publish_remote),
                },
                current,
                source: DriftSource::PublishRemote,
                is_metadata_rewrite,
            },
        );
    }
}

/// Hold one release ref against the commits knives wrote under its name.
///
/// At the newest write it is silent. A commit with that write's change id,
/// parents and tree is the write re-signed or metaedited — routine before a
/// push, since knives writes unsigned commits. The publish remote at any
/// earlier write of the name (or a re-signed copy of one) is knives' own work
/// that a newer write has not been pushed over yet. Both are notes. Anything
/// else is drift, a local name at an earlier write included: it is not what
/// knives last wrote, whether it was moved back or the newer write came from
/// another machine and is not fetched here.
///
/// Only the newest write must be readable: an older one whose commit never
/// reached this machine — the ledger is shared — is skipped.
fn add_release_drift(report: &mut Report, drift: ReleaseDrift<'_>) {
    let ReleaseDrift {
        entries,
        reference,
        current,
        source,
        is_metadata_rewrite,
    } = drift;
    let branch = reference.branch();
    let writes: Vec<_> = crate::release_model::recorded_writes(entries, branch.as_str()).collect();
    let Some(newest) = writes.first() else {
        report.notes.push(format!(
            "{source} {branch} has no recorded cut or edit event"
        ));
        return;
    };
    let accepted = if source == DriftSource::PublishRemote {
        writes.len()
    } else {
        1
    };
    let candidates = writes.iter().take(accepted).enumerate();
    let mut unreadable = None;
    let matched = candidates
        .clone()
        .find(|(_, write)| same_commit(write.commit.as_str(), current.as_str()))
        .map(|(position, write)| (position, write, false))
        .or_else(|| {
            candidates.clone().find_map(|(position, write)| {
                match is_metadata_rewrite(write.commit.as_str(), current.as_str()) {
                    Ok(rewritten) => rewritten.then_some((position, write, true)),
                    Err(error) => {
                        if position == 0 {
                            unreadable = Some(error);
                        }
                        None
                    }
                }
            })
        });
    let (newest_kind, newest_commit) = (newest.kind, newest.commit.short());
    if let Some((position, write, rewritten)) = matched {
        let how = if rewritten {
            " re-signed or metaedited (same change, parents and tree)"
        } else {
            ""
        };
        match position {
            0 if !rewritten => {}
            0 => report.notes.push(format!(
                "{source} {branch} is at {}, the recorded {} {}{how}",
                current.short(),
                write.kind,
                write.commit.short(),
            )),
            _ => report.notes.push(format!(
                "{source} {branch} is at {}, the earlier recorded {} {}{how}; the newest \
                 recorded {newest_kind} names {newest_commit}",
                current.short(),
                write.kind,
                write.commit.short(),
            )),
        }
        return;
    }
    if let Some(error) = unreadable {
        report.problems.push(format!(
            "could not compare {source} {branch} at {} with its recorded writes: {error}",
            current.short(),
        ));
    }
    let detail = format!(
        "{source} {branch} is at {} but its newest recorded {newest_kind} names {newest_commit}",
        current.short(),
    );
    report.findings.push(Finding::new(
        FindingKind::ReleaseDrift,
        Subject::Bookmark(reference),
        detail,
    ));
}

fn add_misplaced_origin_release_refs(
    report: &mut Report,
    origin: &BTreeMap<String, CommitId>,
    scheme: &crate::ids::ReleaseScheme,
    publish_remote: &str,
) {
    if publish_remote == "origin" {
        return;
    }
    for (reference, commit) in origin {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        let branch = BranchName::new(name);
        if !is_release_name(&branch, scheme) {
            continue;
        }
        report.findings.push(Finding::new(
            FindingKind::RemoteDrift,
            Subject::Bookmark(BookmarkRef::Remote {
                branch,
                remote: crate::ids::RemoteName::new("origin"),
            }),
            format!(
                "origin has release ref {name} at {} but releases publish to {publish_remote}; \
                 this ref is misplaced",
                commit.short()
            ),
        ));
    }
}

fn same_commit(recorded: &str, current: &str) -> bool {
    current.starts_with(recorded)
}

fn add_orphan_commits(report: &mut Report, opened: &Repo, path: &Path) {
    let commits = match jj::commits_matching(path, ORPHAN_REVSET) {
        Ok(commits) => commits,
        Err(error) => {
            report
                .problems
                .push(format!("could not list anonymous heads: {error}"));
            return;
        }
    };
    for commit in commits {
        match opened.description_of(commit.as_str()) {
            Ok(description) => report.findings.push(Finding::new(
                FindingKind::OrphanCommit,
                Subject::Commit(commit),
                format!(
                    "anonymous head: {}",
                    description
                        .lines()
                        .next()
                        .map_or("(no description)", str::trim)
                ),
            )),
            Err(error) => report
                .problems
                .push(format!("could not read anonymous head {commit}: {error}")),
        }
    }
}

fn add_open_pull_head_checks(report: &mut Report, input: &AuditInput<'_>, facts: &LocalFacts<'_>) {
    let Some(forge) = input.forge else {
        report
            .problems
            .push("open pull-head reconciliation was skipped (--no-github)".to_owned());
        return;
    };
    if let Err(error) = pull_head_findings(input, forge, facts, report) {
        report
            .problems
            .push(format!("could not read open pull-request heads: {error}"));
    }
}

/// One live batch over the open pull requests we own and the tracked numbers,
/// then: head-position findings per open pull of ours, a problem per open pull
/// of ours the batch withheld, the cache write, and a [`PullSnapshot`] on every
/// branch row whose primary pull request the batch answered open.
fn pull_head_findings(
    input: &AuditInput<'_>,
    forge: &dyn Forge,
    facts: &LocalFacts<'_>,
    report: &mut Report,
) -> Result<(), crate::forge::ForgeError> {
    let fork = input.fork;
    let entry = fork.entry;
    let opened = snapshot::open(SnapshotConfig {
        forge,
        path: &fork.checkout.path,
        remotes: [entry.remote(Role::Origin), entry.remote(Role::Release)],
        cache_root: input.cache_root,
    })?;
    let snapshot = opened.complete_with(facts.tracked, |discovery, tracked| {
        discovery
            .ours()
            .iter()
            .filter(|pull| pull.is_open())
            .map(|pull| pull.number)
            .chain(tracked.values().copied())
            .collect()
    })?;
    // Findings and problems concern the pull requests we own. A tracked number
    // another author opened has a head on their fork, which no local bookmark
    // or origin ref of ours is expected to match: its facts go on the row
    // (`head_matches_tip`), and facts never move the exit.
    let ours_open: BTreeSet<u64> = snapshot
        .ours()
        .iter()
        .filter(|pull| pull.is_open())
        .map(|pull| pull.number)
        .collect();
    for number in snapshot.requested() {
        match snapshot.fact(*number) {
            Some(fact) if fact.pull.is_open() && ours_open.contains(number) => {
                pull_position_findings(
                    &fact.pull,
                    facts.local,
                    facts.origin_refs,
                    &mut report.findings,
                );
            }
            Some(_) => {}
            None if ours_open.contains(number) => {
                report.problems.push(format!(
                    "open pull request #{number} was not answered by the live batch"
                ));
            }
            None => report.notes.push(format!(
                "tracked pull request #{number} was not answered by the live batch"
            )),
        }
    }
    attach_pulls(
        &mut report.branches,
        &snapshot,
        facts.tracked,
        facts.template,
    );
    if let Err(note) = snapshot.persist(None) {
        report.notes.push(note.to_string());
    }
    Ok(())
}

fn pull_position_findings(
    pull: &PullRequest,
    local: &BTreeMap<BranchName, CommitId>,
    origin_refs: &BTreeMap<String, CommitId>,
    findings: &mut Vec<Finding>,
) {
    let branch = BranchName::new(&pull.head_ref_name);
    let remote = origin_refs.get(&format!("refs/heads/{branch}"));
    let expected = pull.head_ref_oid.as_str();
    if remote.is_none_or(|commit| commit.as_str() != expected) {
        findings.push(pull_position_finding(
            pull,
            &format!("origin/{branch}"),
            remote.map_or("missing", CommitId::as_str),
        ));
    }
    let local = local.get(&branch);
    if local.is_none_or(|commit| commit.as_str() != expected) {
        findings.push(pull_position_finding(
            pull,
            "local bookmark",
            local.map_or("missing", CommitId::as_str),
        ));
    }
}

fn pull_position_finding(pull: &PullRequest, position: &str, actual: &str) -> Finding {
    Finding::new(
        FindingKind::RemoteDrift,
        Subject::PullRequest(pull.number),
        format!(
            "pr #{} head {} but {position} is {}",
            pull.number,
            short_id(&pull.head_ref_oid),
            short_id(actual)
        ),
    )
}

/// The command outcome, with unanswered reads outranking detected drift.
pub const fn exit_for(report: &Report) -> Exit {
    if !report.problems.is_empty() {
        Exit::Incomplete
    } else if report.findings.is_empty() {
        Exit::Ok
    } else {
        Exit::Findings
    }
}

/// Render problems, the per-branch facts, findings grouped by detector, notes,
/// and the one deeper-history surface.
pub fn render(report: &Report) -> String {
    let mut lines = format!("{}: audit", report.repo);
    // A problem can name a path from disk or from another machine's ledger ref.
    for problem in &report.problems {
        let _ = write!(
            lines,
            "\n  PROBLEM: {}",
            crate::ledger::inline_human_text(problem)
        );
    }
    if !report.branches.is_empty() {
        lines.push_str("\n  branches:");
        for row in &report.branches {
            let _ = write!(lines, "\n    {}", render_branch(row));
        }
    }
    let mut groups: BTreeMap<FindingKind, Vec<&Finding>> = BTreeMap::new();
    for finding in &report.findings {
        groups.entry(finding.kind).or_default().push(finding);
    }
    for (kind, findings) in groups {
        for finding in findings {
            let _ = write!(
                lines,
                "\n  {kind}: {} — {}",
                finding.subject.short(),
                finding.detail
            );
        }
    }
    for note in &report.notes {
        let _ = write!(lines, "\n  note: {note}");
    }
    lines.push_str("\n  timeline archaeology: knives pr <n> --timeline");
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        AuditInput, LocalFacts, MetadataRewrite, ReleaseDriftScan, Report,
        add_misplaced_origin_release_refs, add_open_pull_head_checks, add_release_drifts, exit_for,
        pull_head_findings, pull_position_findings, same_commit,
    };
    use crate::bind::Fork;
    use crate::cli::Exit;
    use crate::config::RepoEntry;
    use crate::detect::{FindingKind, Subject};
    use crate::forge::{
        Account, Forge, ForgeError, PullDetails, PullFacts, PullRequest, PullSummary, RepoIdentity,
        SweepPage, TimelineEvent, fake::FakeForge,
    };
    use crate::ids::{BookmarkRef, BranchName, CommitId};
    use crate::ledger::{Entry, Kind, Ledger};
    use crate::release_model::{RecordedWrite, RecordedWriteKind, recorded_writes};
    use crate::store::Store;
    use std::collections::BTreeMap;
    use std::path::Path;

    static NO_TIPS: BTreeMap<BranchName, CommitId> = BTreeMap::new();
    static NO_REFS: BTreeMap<String, CommitId> = BTreeMap::new();
    static NO_TRACKED: BTreeMap<BranchName, u64> = BTreeMap::new();

    /// A checkout with no bookmarks, no origin refs, nothing tracked, no template.
    fn no_local_facts() -> LocalFacts<'static> {
        LocalFacts {
            local: &NO_TIPS,
            origin_refs: &NO_REFS,
            tracked: &NO_TRACKED,
            template: None,
        }
    }

    /// Local facts over `local` and `origin`, with `tracked` numbers and no template.
    fn facts_over<'a>(
        local: &'a BTreeMap<BranchName, CommitId>,
        origin: &'a BTreeMap<String, CommitId>,
        tracked: &'a BTreeMap<BranchName, u64>,
    ) -> LocalFacts<'a> {
        LocalFacts {
            local,
            origin_refs: origin,
            tracked,
            template: None,
        }
    }

    /// The audit's dependencies with one scan thread and no cache.
    fn audit_input<'a>(
        fork: &'a Fork<'a>,
        store: &'a Store,
        forge: Option<&'a dyn Forge>,
        cache_root: Option<&'a Path>,
    ) -> AuditInput<'a> {
        AuditInput {
            fork,
            store,
            forge,
            cache_root,
            workers: 1,
        }
    }

    /// One live batch against `forge` over an empty store and no cache.
    fn batch(
        fork: &Fork<'_>,
        forge: &dyn Forge,
        facts: &LocalFacts<'_>,
        report: &mut Report,
    ) -> Result<(), ForgeError> {
        let temp = tempfile::tempdir().expect("create test store");
        let store = Store::open(
            temp.path().join("state.json"),
            &crate::config::FormerNames::new(),
            &[&fork.upstream],
        )
        .expect("open test store");
        pull_head_findings(
            &audit_input(fork, &store, Some(forge), None),
            forge,
            facts,
            report,
        )
    }

    #[derive(Debug)]
    struct ChangingFactsForge {
        discovery: PullRequest,
        fact: Option<PullRequest>,
    }

    impl Forge for ChangingFactsForge {
        fn repo_identity(&self, _repo: &Path) -> Result<RepoIdentity, ForgeError> {
            Ok(RepoIdentity {
                name_with_owner: "fake-owner/fake-repo".to_owned(),
                id: "FAKEID".to_owned(),
            })
        }

        fn list_pull_requests(
            &self,
            _repo: &Path,
            _authors: &[String],
        ) -> Result<Vec<PullSummary>, ForgeError> {
            Ok(vec![PullSummary::of(&self.discovery)])
        }

        fn sweep(&self, _repo: &Path, _target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
            Ok(SweepPage {
                entries: Vec::new(),
                has_next_page: false,
            })
        }

        fn pull_facts(
            &self,
            _repo: &Path,
            _target: &RepoIdentity,
            numbers: &[u64],
        ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
            Ok(self
                .fact
                .as_ref()
                .filter(|pull| numbers.contains(&pull.number))
                .map(|pull| {
                    (
                        pull.number,
                        PullFacts {
                            pull: pull.clone(),
                            details: PullDetails::default(),
                            newest_comment: None,
                        },
                    )
                })
                .into_iter()
                .collect())
        }

        fn pull_timeline(
            &self,
            _repo: &Path,
            _target: &RepoIdentity,
            _number: u64,
        ) -> Result<Vec<TimelineEvent>, ForgeError> {
            Ok(Vec::new())
        }
    }

    fn test_entry() -> RepoEntry {
        RepoEntry::new(
            "git@github.com:upstream/repo.git",
            "git@github.com:owner/fork.git",
        )
    }

    fn test_pull(state: &str) -> PullRequest {
        PullRequest {
            number: 7,
            state: state.to_owned(),
            head_ref_name: "feat/alpha".to_owned(),
            head_ref_oid: "expected0000000000000000000000000000000".to_owned(),
            head_repository_owner: Some(Account {
                login: "owner".to_owned(),
            }),
            ..PullRequest::default()
        }
    }

    #[test]
    fn abbreviated_recorded_commit_matches_its_current_full_identifier() {
        assert!(same_commit(
            "aabbccddeeff",
            "aabbccddeeff00112233445566778899aabbccdd"
        ));
        assert!(!same_commit(
            "aabbccddeeff",
            "ccddeeffaabb00112233445566778899aabbccdd"
        ));
    }

    #[test]
    fn a_later_note_with_commit_evidence_cannot_replace_a_cut_baseline() {
        let entries = vec![
            Entry {
                ts: "2026-08-15T00:00:00Z".to_owned(),
                owner: "test".to_owned(),
                email: None,
                subject: Some("release/2026-08-15".to_owned()),
                kind: Kind::Event,
                disposition: None,
                statement: None,
                text: "cut release/2026-08-15 as aaaaaaaaaaaa with 1 parent(s)".to_owned(),
                evidence: vec!["aaaaaaaaaaaa".to_owned(), "member0000000".to_owned()],
                anchor: None,
                pr: None,
                parents: Vec::new(),
            },
            Entry {
                ts: "2026-08-16T00:00:00Z".to_owned(),
                owner: "test".to_owned(),
                email: None,
                subject: Some("release/2026-08-15".to_owned()),
                kind: Kind::Note,
                disposition: Some("ruled-out".to_owned()),
                statement: None,
                text: "later disposition".to_owned(),
                evidence: vec!["bbbbbbbbbbbb".to_owned()],
                anchor: None,
                pr: None,
                parents: Vec::new(),
            },
        ];

        assert_eq!(
            recorded_writes(&entries, "release/2026-08-15").collect::<Vec<_>>(),
            [RecordedWrite {
                kind: RecordedWriteKind::Cut,
                commit: CommitId::new("aaaaaaaaaaaa"),
            }]
        );
    }

    #[test]
    fn a_later_edit_event_is_the_baseline_a_release_is_held_against() {
        let event = |ts: &str, text: &str, commit: &str| Entry {
            ts: ts.to_owned(),
            owner: "test".to_owned(),
            email: None,
            subject: Some("release/2026-08-15".to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: text.to_owned(),
            evidence: vec![commit.to_owned(), "member0000000".to_owned()],
            anchor: None,
            pr: None,
            parents: Vec::new(),
        };
        let entries = vec![
            event(
                "2026-08-15T00:00:00Z",
                "cut release/2026-08-15 as aaaaaaaaaaaa with 2 parent(s)",
                "aaaaaaaaaaaa",
            ),
            // The event an older knives wrote, before edits recorded a change id.
            event(
                "2026-08-16T00:00:00Z",
                "edited release/2026-08-15: dropped feat/beta: not ready; parents: feat/alpha@m",
                "bbbbbbbbbbbb",
            ),
        ];
        let schemes = [crate::ids::ReleaseScheme::Dated];
        // The local name and the publish remote, both at `commit`.
        let drift_at = |commit: &str, rewrite: &MetadataRewrite<'_>| {
            let local =
                BTreeMap::from([(BranchName::new("release/2026-08-15"), CommitId::new(commit))]);
            let published = BTreeMap::from([(
                "refs/heads/release/2026-08-15".to_owned(),
                CommitId::new(commit),
            )]);
            let mut report = Report::new("demo");
            add_release_drifts(
                &mut report,
                &ReleaseDriftScan {
                    local: &local,
                    published: &published,
                    schemes: &schemes,
                    publish_remote: "release",
                    entries: &entries,
                    is_metadata_rewrite: rewrite,
                },
            );
            report
        };
        let unrelated = |_: &str, _: &str| -> Result<bool, crate::jj::JjError> { Ok(false) };

        let edited = drift_at("bbbbbbbbbbbb000000000000000000000000000000", &unrelated);
        assert!(edited.findings.is_empty(), "{edited:?}");
        assert!(edited.notes.is_empty(), "{edited:?}");
        // The cut the edit replaced: on the publish remote, the edit is not
        // pushed yet; locally, the name was moved back after the edit.
        let cut = drift_at("aaaaaaaaaaaa000000000000000000000000000000", &unrelated);
        assert_eq!(
            cut.notes,
            [
                "publish remote release/2026-08-15 is at aaaaaaaaaaaa, the earlier recorded cut \
                 aaaaaaaaaaaa; the newest recorded edit names bbbbbbbbbbbb"
            ]
        );
        assert_eq!(cut.findings.len(), 1, "{cut:?}");
        assert_eq!(
            cut.findings.first().map(|finding| finding.detail.as_str()),
            Some(
                "local release/2026-08-15 is at aaaaaaaaaaaa but its newest recorded edit names \
                 bbbbbbbbbbbb"
            )
        );
        let elsewhere = drift_at("dddddddddddd000000000000000000000000000000", &unrelated);
        assert_eq!(elsewhere.findings.len(), 2, "{elsewhere:?}");
    }

    #[test]
    fn an_unreadable_older_write_is_skipped_and_an_unreadable_newest_is_a_problem() {
        let event = |text: &str, commit: &str| Entry {
            ts: "2026-08-15T00:00:00Z".to_owned(),
            owner: "test".to_owned(),
            email: None,
            subject: Some("release/2026-08-15".to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: text.to_owned(),
            evidence: vec![commit.to_owned(), "member0000000".to_owned()],
            anchor: None,
            pr: None,
            parents: Vec::new(),
        };
        let entries = vec![
            event(
                "cut release/2026-08-15 as aaaaaaaaaaaa with 2 parent(s)",
                "aaaaaaaaaaaa",
            ),
            event(
                "edited release/2026-08-15 as bbbbbbbbbbbb: x",
                "bbbbbbbbbbbb",
            ),
        ];
        let local = BTreeMap::new();
        let schemes = [crate::ids::ReleaseScheme::Dated];
        let published = BTreeMap::from([(
            "refs/heads/release/2026-08-15".to_owned(),
            CommitId::new("dddddddddddd000000000000000000000000000000"),
        )]);
        let audit = |unreadable: &'static str| {
            let mut report = Report::new("demo");
            add_release_drifts(
                &mut report,
                &ReleaseDriftScan {
                    local: &local,
                    published: &published,
                    schemes: &schemes,
                    publish_remote: "release",
                    entries: &entries,
                    is_metadata_rewrite: &|recorded, _| {
                        if recorded == unreadable {
                            Err(crate::jj::JjError::Revision {
                                revision: recorded.to_owned(),
                                detail: "not found".to_owned(),
                            })
                        } else {
                            Ok(false)
                        }
                    },
                },
            );
            report
        };

        let older = audit("aaaaaaaaaaaa");
        assert_eq!(older.findings.len(), 1, "{older:?}");
        assert!(older.problems.is_empty(), "{older:?}");
        let newest = audit("bbbbbbbbbbbb");
        assert_eq!(newest.findings.len(), 1, "{newest:?}");
        assert_eq!(newest.problems.len(), 1, "{newest:?}");
    }

    #[test]
    fn a_re_signed_copy_of_the_recorded_write_is_a_note_and_an_unreadable_one_a_problem() {
        let entries = vec![Entry {
            ts: "2026-08-16T00:00:00Z".to_owned(),
            owner: "test".to_owned(),
            email: None,
            subject: Some("release/2026-08-15".to_owned()),
            kind: Kind::Event,
            disposition: None,
            statement: None,
            text: "edited release/2026-08-15 as bbbbbbbbbbbb (change zzzz) with 1 parent(s): \
                   dropped feat/beta: x; parents: feat/alpha@m"
                .to_owned(),
            evidence: vec!["bbbbbbbbbbbb".to_owned(), "member0000000".to_owned()],
            anchor: None,
            pr: None,
            parents: Vec::new(),
        }];
        let local = BTreeMap::from([(
            BranchName::new("release/2026-08-15"),
            CommitId::new("cccccccccccc000000000000000000000000000000"),
        )]);
        let published = BTreeMap::new();
        let schemes = [crate::ids::ReleaseScheme::Dated];
        let audit = |rewrite: &MetadataRewrite<'_>| {
            let mut report = Report::new("demo");
            add_release_drifts(
                &mut report,
                &ReleaseDriftScan {
                    local: &local,
                    published: &published,
                    schemes: &schemes,
                    publish_remote: "release",
                    entries: &entries,
                    is_metadata_rewrite: rewrite,
                },
            );
            report
        };

        let signed = audit(&|recorded, current| {
            assert_eq!(recorded, "bbbbbbbbbbbb");
            assert_eq!(current, "cccccccccccc000000000000000000000000000000");
            Ok(true)
        });
        assert!(signed.findings.is_empty(), "{signed:?}");
        assert_eq!(
            signed.notes,
            [
                "local release/2026-08-15 is at cccccccccccc, the recorded edit bbbbbbbbbbbb \
              re-signed or metaedited (same change, parents and tree)"
            ]
        );

        let unreadable = audit(&|_, _| {
            Err(crate::jj::JjError::Revision {
                revision: "bbbbbbbbbbbb".to_owned(),
                detail: "not found".to_owned(),
            })
        });
        assert_eq!(unreadable.findings.len(), 1, "{unreadable:?}");
        assert_eq!(unreadable.problems.len(), 1, "{unreadable:?}");
    }

    #[test]
    fn a_publish_only_release_drift_is_compared_to_its_recorded_cut() {
        let directory = tempfile::tempdir().expect("ledger directory");
        let ledger = Ledger::at(directory.path().to_owned());
        ledger
            .append(&Entry {
                ts: "2026-08-15T00:00:00Z".to_owned(),
                owner: "test".to_owned(),
                email: None,
                subject: Some("release/2026-08-15".to_owned()),
                kind: Kind::Event,
                disposition: None,
                statement: None,
                text: "cut release/2026-08-15 as aaaaaaaaaaaa with 1 parent(s)".to_owned(),
                evidence: vec!["aaaaaaaaaaaa".to_owned(), "bbbbbbbbbbbb".to_owned()],
                anchor: None,
                pr: None,
                parents: Vec::new(),
            })
            .expect("record cut");
        let entries = ledger.read().expect("read the cut back").entries;
        let mut report = Report::new("demo");
        let published = BTreeMap::from([(
            "refs/heads/release/2026-08-15".to_owned(),
            CommitId::new("cccccccccccccccccccccccccccccccccccccccc"),
        )]);

        add_release_drifts(
            &mut report,
            &ReleaseDriftScan {
                local: &BTreeMap::new(),
                published: &published,
                schemes: &[crate::ids::ReleaseScheme::Dated],
                publish_remote: "release",
                entries: &entries,
                is_metadata_rewrite: &|_, _| Ok(false),
            },
        );

        assert_eq!(report.findings.len(), 1, "{report:?}");
        let finding = report.findings.first().expect("one release-drift finding");
        assert_eq!(finding.kind, FindingKind::ReleaseDrift);
        assert_eq!(
            finding.subject,
            Subject::Bookmark(BookmarkRef::Remote {
                branch: BranchName::new("release/2026-08-15"),
                remote: crate::ids::RemoteName::new("release"),
            })
        );
        assert!(
            finding.detail.contains("publish remote"),
            "was: {finding:?}"
        );
    }

    #[test]
    fn an_origin_release_ref_is_misplaced_when_another_remote_publishes_releases() {
        let mut report = Report::new("demo");
        add_misplaced_origin_release_refs(
            &mut report,
            &BTreeMap::from([(
                "refs/heads/release/2026-08-15".to_owned(),
                CommitId::new("aaaaaaaaaaaa"),
            )]),
            &crate::ids::ReleaseScheme::Dated,
            "release",
        );

        assert_eq!(report.findings.len(), 1, "{report:?}");
        let finding = report
            .findings
            .first()
            .expect("one misplaced-origin-release finding");
        assert_eq!(finding.kind, FindingKind::RemoteDrift);
        assert_eq!(
            finding.subject,
            Subject::Bookmark(BookmarkRef::Remote {
                branch: BranchName::new("release/2026-08-15"),
                remote: crate::ids::RemoteName::new("origin"),
            })
        );
    }

    #[test]
    fn an_equal_release_url_does_not_misplace_origin_release_refs() {
        let entry = RepoEntry {
            release: Some("https://forge.invalid/ours/demo.git".to_owned()),
            ..RepoEntry::new(
                "https://forge.invalid/up/demo.git",
                "https://forge.invalid/ours/demo.git",
            )
        };
        let mut report = Report::new("demo");

        add_misplaced_origin_release_refs(
            &mut report,
            &BTreeMap::from([(
                "refs/heads/release/2026-08-15".to_owned(),
                CommitId::new("aaaaaaaaaaaa"),
            )]),
            &crate::ids::ReleaseScheme::Dated,
            entry.publish_remote(),
        );

        assert!(report.findings.is_empty(), "findings: {report:?}");
    }

    #[test]
    fn open_pull_head_drift_distinguishes_origin_and_local_positions() {
        let pull = PullRequest {
            number: 7,
            head_ref_name: "feat/alpha".to_owned(),
            head_ref_oid: "expected0000000000000000000000000000000".to_owned(),
            ..PullRequest::default()
        };
        let local = BTreeMap::from([(
            BranchName::new("feat/alpha"),
            CommitId::new("local000000000000000000000000000000000000"),
        )]);
        let origin = BTreeMap::from([(
            "refs/heads/feat/alpha".to_owned(),
            CommitId::new("origin00000000000000000000000000000000000"),
        )]);
        let mut findings = Vec::new();

        pull_position_findings(&pull, &local, &origin, &mut findings);

        assert_eq!(findings.len(), 2);
        assert!(
            findings
                .iter()
                .all(|finding| finding.subject == Subject::PullRequest(7))
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.detail.contains("origin/feat/alpha"))
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.detail.contains("local bookmark"))
        );
    }

    #[test]
    fn open_pull_check_batches_the_open_owned_pull_before_comparing_positions() {
        let entry = RepoEntry::new(
            "git@github.com:upstream/repo.git",
            "git@github.com:owner/fork.git",
        );
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(
                BranchName::new("feat/alpha"),
                PullRequest {
                    number: 7,
                    state: "OPEN".to_owned(),
                    head_ref_name: "feat/alpha".to_owned(),
                    head_ref_oid: "expected0000000000000000000000000000000".to_owned(),
                    head_repository_owner: Some(Account {
                        login: "owner".to_owned(),
                    }),
                    ..PullRequest::default()
                },
            )]),
            ..FakeForge::default()
        };
        let local = BTreeMap::from([(
            BranchName::new("feat/alpha"),
            CommitId::new("local000000000000000000000000000000000000"),
        )]);
        let origin = BTreeMap::from([(
            "refs/heads/feat/alpha".to_owned(),
            CommitId::new("origin00000000000000000000000000000000000"),
        )]);
        let mut report = Report::new("demo");

        batch(
            &fork,
            &forge,
            &facts_over(&local, &origin, &NO_TRACKED),
            &mut report,
        )
        .expect("fake forge should complete the open pull batch");

        let findings = report.findings;
        assert_eq!(findings.len(), 2);
        assert!(
            findings
                .iter()
                .all(|finding| finding.subject == Subject::PullRequest(7))
        );
    }

    #[test]
    fn open_pull_head_check_persists_its_completed_snapshot() {
        let entry = test_entry();
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(BranchName::new("feat/alpha"), test_pull("OPEN"))]),
            ..FakeForge::default()
        };
        let temp = tempfile::tempdir().expect("create test cache");
        let store = Store::open(
            temp.path().join("state.json"),
            &crate::config::FormerNames::new(),
            &[&fork.upstream],
        )
        .expect("open test store");
        let mut report = Report::new("demo");

        add_open_pull_head_checks(
            &mut report,
            &audit_input(&fork, &store, Some(&forge), Some(temp.path())),
            &no_local_facts(),
        );

        let identity = RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        };
        let cache = crate::forge_cache::cache_path(temp.path(), &identity)
            .and_then(|path| crate::forge_cache::load(&path, &identity));
        assert!(cache.is_some(), "completed snapshot was not persisted");
    }

    #[test]
    fn open_pull_head_check_notes_a_completed_snapshot_cache_write_failure() {
        let entry = test_entry();
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(BranchName::new("feat/alpha"), test_pull("OPEN"))]),
            ..FakeForge::default()
        };
        let temp = tempfile::tempdir().expect("create test cache");
        let blocked_root = temp.path().join("blocked-cache-root");
        std::fs::write(&blocked_root, "not a directory").expect("block cache root");
        let store = Store::open(
            temp.path().join("state.json"),
            &crate::config::FormerNames::new(),
            &[&fork.upstream],
        )
        .expect("open test store");
        let mut report = Report::new("demo");

        add_open_pull_head_checks(
            &mut report,
            &audit_input(&fork, &store, Some(&forge), Some(&blocked_root)),
            &no_local_facts(),
        );

        assert!(
            report
                .notes
                .iter()
                .any(|note| note.starts_with("forge cache not saved: ")),
            "cache-write failure was not noted: {report:?}"
        );
    }

    #[test]
    fn a_withheld_open_pull_fact_makes_the_audit_incomplete() {
        let entry = test_entry();
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let forge = ChangingFactsForge {
            discovery: test_pull("OPEN"),
            fact: None,
        };
        let temp = tempfile::tempdir().expect("create test store");
        let store = Store::open(
            temp.path().join("state.json"),
            &crate::config::FormerNames::new(),
            &[&fork.upstream],
        )
        .expect("open test store");
        let mut report = Report::new("demo");

        add_open_pull_head_checks(
            &mut report,
            &audit_input(&fork, &store, Some(&forge), None),
            &no_local_facts(),
        );

        assert_eq!(
            report.problems,
            vec!["open pull request #7 was not answered by the live batch"]
        );
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }

    #[test]
    fn pull_that_closes_during_the_live_batch_has_no_head_drift() {
        let entry = test_entry();
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let forge = ChangingFactsForge {
            discovery: test_pull("OPEN"),
            fact: Some(test_pull("CLOSED")),
        };
        let local = BTreeMap::from([(
            BranchName::new("feat/alpha"),
            CommitId::new("local000000000000000000000000000000000000"),
        )]);
        let origin = BTreeMap::from([(
            "refs/heads/feat/alpha".to_owned(),
            CommitId::new("origin00000000000000000000000000000000000"),
        )]);
        let mut report = Report::new("demo");

        batch(
            &fork,
            &forge,
            &facts_over(&local, &origin, &NO_TRACKED),
            &mut report,
        )
        .expect("the changed pull fact is still a successful batch answer");

        assert!(report.findings.is_empty(), "was: {:?}", report.findings);
    }

    #[test]
    fn a_tracked_pull_of_another_author_does_not_move_the_exit() {
        // Given: `knives track --pr 41` on feat/alpha names a pull request someone
        // else opened from their fork, whose head equals our local tip.
        let entry = test_entry();
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let tip = "expected0000000000000000000000000000000";
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(
                BranchName::new("their/branch"),
                PullRequest {
                    number: 41,
                    state: "OPEN".to_owned(),
                    head_ref_name: "their/branch".to_owned(),
                    head_ref_oid: tip.to_owned(),
                    head_repository_owner: Some(Account {
                        login: "someone-else".to_owned(),
                    }),
                    ..PullRequest::default()
                },
            )]),
            ..FakeForge::default()
        };
        let local = BTreeMap::from([(BranchName::new("feat/alpha"), CommitId::new(tip))]);
        let tracked = BTreeMap::from([(BranchName::new("feat/alpha"), 41)]);
        let mut report = Report::new("demo");

        batch(
            &fork,
            &forge,
            &facts_over(&local, &NO_REFS, &tracked),
            &mut report,
        )
        .expect("the tracked number is answered");

        // Then: nothing moves — their branch name is not ours to reconcile
        // against origin or a bookmark.
        assert!(report.findings.is_empty(), "was: {:?}", report.findings);
        assert!(report.problems.is_empty(), "was: {:?}", report.problems);
        assert_eq!(exit_for(&report), Exit::Ok);
    }

    #[test]
    fn an_unanswered_tracked_number_is_a_note_not_a_problem() {
        // Given: a tracked number the forge no longer knows (deleted, or mistyped).
        let entry = test_entry();
        let fork = Fork::at("demo", &entry, Path::new("/fake"));
        let forge = FakeForge::default();
        let local = BTreeMap::from([(
            BranchName::new("feat/alpha"),
            CommitId::new("local000000000000000000000000000000000000"),
        )]);
        let tracked = BTreeMap::from([(BranchName::new("feat/alpha"), 41)]);
        let mut report = Report::new("demo");

        batch(
            &fork,
            &forge,
            &facts_over(&local, &NO_REFS, &tracked),
            &mut report,
        )
        .expect("an unanswered tracked number is not a batch failure");

        assert_eq!(
            report.notes,
            vec!["tracked pull request #41 was not answered by the live batch"]
        );
        assert!(report.problems.is_empty(), "was: {:?}", report.problems);
        assert_eq!(exit_for(&report), Exit::Ok);
    }
}
