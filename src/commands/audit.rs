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
    BookmarkRef, BranchName, BranchTarget, CommitId, RepoName, is_release_name, short_id,
};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, Ledger};
use crate::snapshot::{self, SnapshotConfig};
use crate::store::Store;

const ORPHAN_REVSET: &str = r#"heads(all()) ~ ::(bookmarks() | remote_bookmarks() | tags()) ~ working_copies() ~ (empty() & description(exact:""))"#;

/// Every audit finding, note, and unanswered check for one repository.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    /// One row per maintained branch: what is observed, never what to do.
    pub branches: Vec<BranchFacts>,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Facts about one maintained branch and its pull request. Facts, never
/// verdicts: each field is an observation, and `None` means unobserved.
#[derive(Debug, serde::Serialize)]
pub struct BranchFacts {
    pub branch: String,
    pub tip: String,
    /// Where origin holds the branch; `None` when origin has no such ref.
    pub origin_tip: Option<String>,
    pub tip_matches_origin: Option<bool>,
    /// Stated with `knives track --fork-only`: no pull request is expected and
    /// the forbidden-identifier scan does not apply.
    pub fork_only: bool,
    /// Absent when no open pull request was answered for this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull: Option<PullSnapshot>,
    /// Absent when no `forbidden` list is configured, the branch is fork-only,
    /// or the diff could not be read (a `problems` line names the branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbidden: Option<Vec<crate::forbidden::Hit>>,
}

/// The open pull request on a maintained branch, as the live batch answered it.
#[derive(Debug, serde::Serialize)]
pub struct PullSnapshot {
    pub number: u64,
    pub state: String,
    pub url: String,
    pub head: String,
    pub head_matches_tip: bool,
    pub mergeable: Option<String>,
    pub merge_state_status: Option<String>,
    pub review_decision: String,
    pub checks: Option<CheckCounts>,
    pub unresolved_review_threads: Option<usize>,
    /// `None` when upstream's trunk has no pull-request template or the batch
    /// did not answer the body.
    pub template: Option<TemplateFacts>,
}

/// Check runs on the pull request's head, counted by conclusion.
#[derive(Debug, serde::Serialize)]
pub struct CheckCounts {
    pub total: usize,
    pub pending: usize,
    pub conclusions: BTreeMap<String, usize>,
}

/// Upstream's pull-request template held against the pull request's body.
#[derive(Debug, serde::Serialize)]
pub struct TemplateFacts {
    pub file: String,
    pub headings: Vec<String>,
    pub missing_from_body: Vec<String>,
}

/// Upstream's pull-request template, read once per run.
#[derive(Debug)]
struct Template {
    file: String,
    headings: Vec<String>,
}

impl Template {
    /// The headings the body does not carry: a body line whose `#`-stripped,
    /// trimmed text equals the heading case-insensitively carries it.
    fn against(&self, body: &str) -> TemplateFacts {
        let carried: Vec<String> = body
            .lines()
            .filter_map(heading_text)
            .map(str::to_lowercase)
            .collect();
        TemplateFacts {
            file: self.file.clone(),
            headings: self.headings.clone(),
            missing_from_body: self
                .headings
                .iter()
                .filter(|heading| !carried.contains(&heading.to_lowercase()))
                .cloned()
                .collect(),
        }
    }
}

/// A Markdown ATX heading's text: `^#{1,6}\s+(.+)$`, trimmed.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line.get(hashes..)?;
    if !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim();
    (!text.is_empty()).then_some(text)
}

/// The first pull-request template among the convention files that upstream's
/// trunk carries, or `None` when it carries none.
fn read_template(path: &Path, entry: &RepoEntry) -> Result<Option<Template>, jj::JjError> {
    let trunk = entry.upstream_trunk();
    for file in crate::commands::preflight::CONVENTION_FILES
        .iter()
        .filter(|file| file.to_ascii_lowercase().contains("pull_request_template"))
    {
        if let Some(text) = jj::file_text(path, &trunk, file)? {
            return Ok(Some(Template {
                file: (*file).to_owned(),
                headings: text
                    .lines()
                    .filter_map(heading_text)
                    .map(str::to_owned)
                    .collect(),
            }));
        }
    }
    Ok(None)
}

/// Dependencies shared by the read-only estate checks.
pub struct AuditInput<'a> {
    pub fork: &'a Fork<'a>,
    pub store: &'a Store,
    pub forge: Option<&'a dyn Forge>,
    pub cache_root: Option<&'a Path>,
}

impl std::fmt::Debug for AuditInput<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditInput")
            .field("fork", self.fork)
            .field("forge", &self.forge.is_some())
            .field("cache_root", &self.cache_root)
            .finish()
    }
}

/// Gather the estate facts without writing a repository, remote, store, or ledger.
pub fn gather(input: &AuditInput<'_>) -> Report {
    let fork = input.fork;
    let entry = fork.entry;
    let path = &fork.checkout.path;
    let mut report = Report {
        repo: fork.name.to_string(),
        branches: Vec::new(),
        findings: Vec::new(),
        notes: Vec::new(),
        problems: Vec::new(),
    };
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
    add_unconfigured_remote_refs(&mut report, &opened, &fork.checkout.remotes, &tips);
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
    let scheme = entry.release_scheme();
    let requested: Vec<BranchName> = local.keys().cloned().collect();
    let tracked = tracked(input.store, &fork.name, &requested);
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
        repo: &fork.name,
        local: &local,
        live: &live,
        scheme: &scheme,
    }));
    add_release_drifts(
        &mut report,
        &ReleaseDriftScan {
            local: &local,
            published: live.release(),
            scheme: &scheme,
            publish_remote: entry.publish_remote(),
            ledger: &Ledger::for_repo(&fork.name),
        },
    );
    add_misplaced_origin_release_refs(&mut report, live.origin(), &scheme, entry.publish_remote());
    add_orphan_commits(&mut report, &opened, path);
    // Upstream's template is only held against pull bodies, so it is read only
    // when a forge will answer them.
    let template = match input.forge.map(|_| read_template(path, entry)) {
        Some(Ok(template)) => template,
        Some(Err(error)) => {
            report.problems.push(format!(
                "could not read the pull-request template at {}: {error}",
                entry.upstream_trunk()
            ));
            None
        }
        None => None,
    };
    let facts = LocalFacts {
        local: &local,
        origin_refs: live.origin(),
        scheme: &scheme,
        tracked: &tracked,
        template: template.as_ref(),
    };
    add_branch_facts(&mut report, input, &facts);
    add_open_pull_head_checks(&mut report, input, &facts);
    report
}

/// What the checkout, the live origin refs and the store say, before any forge
/// is asked.
struct LocalFacts<'a> {
    local: &'a BTreeMap<BranchName, CommitId>,
    origin_refs: &'a BTreeMap<String, CommitId>,
    scheme: &'a crate::ids::ReleaseScheme,
    /// Pull numbers the store tracks per branch.
    tracked: &'a BTreeMap<BranchName, u64>,
    /// Upstream's pull-request template, when its trunk carries one.
    template: Option<&'a Template>,
}

/// One row per maintained branch — every local bookmark that is neither the
/// trunk nor a release — with its local facts and, when configured and not
/// exempt, the forbidden-identifier scan of its diff over upstream's trunk.
fn add_branch_facts(report: &mut Report, input: &AuditInput<'_>, facts: &LocalFacts<'_>) {
    let fork = input.fork;
    let entry = fork.entry;
    let mut rows: Vec<BranchFacts> = facts
        .local
        .iter()
        .filter(|(branch, _)| {
            !is_release_name(branch, facts.scheme) && branch.as_str() != entry.trunk()
        })
        .map(|(branch, tip)| {
            let origin_tip = facts.origin_refs.get(&format!("refs/heads/{branch}"));
            BranchFacts {
                branch: branch.to_string(),
                tip: tip.to_string(),
                origin_tip: origin_tip.map(ToString::to_string),
                tip_matches_origin: origin_tip.map(|origin| origin == tip),
                fork_only: input
                    .store
                    .is_fork_only(&BranchTarget::new(fork.name.clone(), branch.clone())),
                pull: None,
                forbidden: None,
            }
        })
        .collect();
    if !entry.forbidden.is_empty() {
        let scanned: Vec<&str> = facts
            .local
            .keys()
            .map(BranchName::as_str)
            .filter(|branch| {
                rows.iter()
                    .any(|row| row.branch == *branch && !row.fork_only)
            })
            .collect();
        let mut results = forbidden_scans(&fork.checkout.path, entry, &scanned);
        for row in &mut rows {
            match results.remove(row.branch.as_str()) {
                Some(Ok(hits)) => row.forbidden = Some(hits),
                Some(Err(problem)) => report.problems.push(problem),
                None => {}
            }
        }
    }
    report.branches.extend(rows);
}

/// The forbidden-identifier scan of every named branch's diff over upstream's
/// trunk, one `jj diff` subprocess per branch, run across as many threads as
/// the machine offers: fifty branches at two hundred milliseconds each is ten
/// seconds serial. An unreadable diff is a problem line naming the branch.
fn forbidden_scans<'b>(
    path: &Path,
    entry: &RepoEntry,
    branches: &[&'b str],
) -> BTreeMap<&'b str, Result<Vec<crate::forbidden::Hit>, String>> {
    if branches.is_empty() {
        return BTreeMap::new();
    }
    let upstream_trunk = entry.upstream_trunk();
    let workers = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .clamp(1, branches.len());
    let chunk = branches.len().div_ceil(workers);
    std::thread::scope(|scope| {
        // Collected on purpose: every worker must be spawned before the first
        // join, or the chunks run one after another on this thread.
        #[allow(
            clippy::needless_collect,
            reason = "spawn every chunk before joining any; a lazy chain would serialise them"
        )]
        let handles: Vec<_> = branches
            .chunks(chunk)
            .map(|slice| {
                let upstream_trunk = upstream_trunk.as_str();
                let handle = scope.spawn(move || {
                    slice
                        .iter()
                        .map(|branch| {
                            let scan = jj::diff_git(path, upstream_trunk, branch)
                                .map(|diff| crate::forbidden::scan(&diff, &entry.forbidden))
                                .map_err(|error| {
                                    format!(
                                        "could not diff {branch} against {upstream_trunk} for the forbidden scan: {error}"
                                    )
                                });
                            (*branch, scan)
                        })
                        .collect::<Vec<_>>()
                });
                (slice, handle)
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|(slice, handle)| {
                handle.join().unwrap_or_else(|_| {
                    slice
                        .iter()
                        .map(|branch| {
                            (
                                *branch,
                                Err(format!("forbidden scan task panicked for {branch}")),
                            )
                        })
                        .collect()
                })
            })
            .collect()
    })
}

fn add_unconfigured_remote_refs(
    report: &mut Report,
    opened: &Repo,
    configured: &BTreeMap<String, String>,
    tips: &BookmarkTips,
) {
    let conflicted = match opened.conflicted_bookmarks() {
        Ok(conflicted) => conflicted,
        Err(error) => {
            report
                .problems
                .push(format!("could not read conflicted bookmarks: {error}"));
            return;
        }
    };
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

fn tracked(store: &Store, repo: &RepoName, branches: &[BranchName]) -> BTreeMap<BranchName, u64> {
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
    repo: &'a RepoName,
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
    scheme: &'a crate::ids::ReleaseScheme,
    publish_remote: &'a str,
    ledger: &'a Ledger,
}

struct ReleaseDrift<'a> {
    entries: &'a [Entry],
    reference: BookmarkRef,
    current: &'a CommitId,
    source: &'a str,
}

fn add_release_drifts(report: &mut Report, scan: &ReleaseDriftScan<'_>) {
    let ReleaseDriftScan {
        local,
        published,
        scheme,
        publish_remote,
        ledger,
    } = *scan;
    let entries = match ledger.entries() {
        Ok(entries) => entries,
        Err(error) => {
            report
                .problems
                .push(format!("could not read ledger: {error}"));
            return;
        }
    };
    for (branch, current) in local
        .iter()
        .filter(|(branch, _)| is_release_name(branch, scheme))
    {
        add_release_drift(
            report,
            ReleaseDrift {
                entries: &entries,
                reference: BookmarkRef::Local(branch.clone()),
                current,
                source: "local",
            },
        );
    }
    for (reference, current) in published {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        let branch = BranchName::new(name);
        if !is_release_name(&branch, scheme) {
            continue;
        }
        add_release_drift(
            report,
            ReleaseDrift {
                entries: &entries,
                reference: BookmarkRef::Remote {
                    branch,
                    remote: crate::ids::RemoteName::new(publish_remote),
                },
                current,
                source: "publish remote",
            },
        );
    }
}

fn add_release_drift(report: &mut Report, drift: ReleaseDrift<'_>) {
    let ReleaseDrift {
        entries,
        reference,
        current,
        source,
    } = drift;
    let branch = reference.branch();
    match recorded_commit(entries, branch.as_str()) {
        Some(recorded) if !same_commit(recorded.as_str(), current.as_str()) => {
            let detail = format!(
                "{source} {branch} is at {} but its newest recorded cut names {}",
                current.short(),
                recorded.short(),
            );
            report.findings.push(Finding::new(
                FindingKind::ReleaseDrift,
                Subject::Bookmark(reference),
                detail,
            ));
        }
        Some(_) => {}
        None => report
            .notes
            .push(format!("{source} {branch} has no recorded cut event")),
    }
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

fn recorded_commit(entries: &[Entry], subject: &str) -> Option<CommitId> {
    crate::release_model::last_recorded_cut(entries, Some(subject)).map(|cut| cut.commit)
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
    let request = PullHeadInput {
        fork: input.fork,
        forge,
        cache_root: input.cache_root,
        local: facts.local,
        origin_refs: facts.origin_refs,
        tracked: facts.tracked,
        template: facts.template,
    };
    if let Err(error) = pull_head_findings(&request, report) {
        report
            .problems
            .push(format!("could not read open pull-request heads: {error}"));
    }
}

struct PullHeadInput<'a> {
    fork: &'a Fork<'a>,
    forge: &'a dyn Forge,
    cache_root: Option<&'a Path>,
    local: &'a BTreeMap<BranchName, CommitId>,
    origin_refs: &'a BTreeMap<String, CommitId>,
    /// Pull numbers the store tracks per branch, asked for beside the open
    /// ones: a tracked pull request another owner submitted is not discovered
    /// as ours, and its head is still the head to compare.
    tracked: &'a BTreeMap<BranchName, u64>,
    /// Upstream's pull-request template, when its trunk carries one.
    template: Option<&'a Template>,
}

/// One live batch over the open pull requests we own and the tracked numbers,
/// then: head-position findings per open pull of ours, a problem per open pull
/// of ours the batch withheld, the cache write, and a [`PullSnapshot`] on every
/// branch row whose primary pull request the batch answered open.
fn pull_head_findings(
    input: &PullHeadInput<'_>,
    report: &mut Report,
) -> Result<(), crate::forge::ForgeError> {
    let entry = input.fork.entry;
    let opened = snapshot::open(SnapshotConfig {
        forge: input.forge,
        path: &input.fork.checkout.path,
        remotes: [entry.remote(Role::Origin), entry.remote(Role::Release)],
        cache_root: input.cache_root,
    })?;
    let snapshot = opened.complete_with(input.tracked, |discovery, tracked| {
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
                    input.local,
                    input.origin_refs,
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
    for row in &mut report.branches {
        let branch = BranchName::new(row.branch.as_str());
        // The branch's primary pull request by the crate's one rule (open beats
        // closed); a tracked number stands in when discovery did not list one.
        let number = snapshot
            .index()
            .by_branch
            .get(&branch)
            .filter(|primary| primary.is_open())
            .map(|primary| primary.number)
            .or_else(|| input.tracked.get(&branch).copied());
        row.pull = number
            .and_then(|number| snapshot.fact(number))
            .filter(|fact| fact.pull.is_open())
            .map(|fact| pull_snapshot(fact, row.tip.as_str(), input.template));
    }
    if let Err(note) = snapshot.persist(None) {
        report.notes.push(note.to_string());
    }
    Ok(())
}

/// The pull-request facts the live batch answered, counted and held against
/// the branch tip and upstream's template.
fn pull_snapshot(
    fact: &crate::forge::PullFacts,
    tip: &str,
    template: Option<&Template>,
) -> PullSnapshot {
    let pull = &fact.pull;
    let details = &fact.details;
    let checks = details.checks.as_ref().map(|checks| {
        let mut conclusions: BTreeMap<String, usize> = BTreeMap::new();
        for conclusion in checks
            .runs
            .iter()
            .filter_map(|run| run.conclusion.as_deref())
        {
            *conclusions.entry(conclusion.to_uppercase()).or_default() += 1;
        }
        CheckCounts {
            total: checks.runs.len(),
            pending: checks
                .runs
                .iter()
                .filter(|run| run.conclusion.is_none())
                .count(),
            conclusions,
        }
    });
    PullSnapshot {
        number: pull.number,
        state: pull.state.clone(),
        url: pull.url.clone(),
        head: pull.head_ref_oid.clone(),
        head_matches_tip: pull.head_ref_oid == tip,
        mergeable: pull.mergeable.clone(),
        merge_state_status: pull.merge_state_status.clone(),
        review_decision: pull.review_decision.clone(),
        checks,
        unresolved_review_threads: details.unresolved_review_threads,
        template: match (template, details.body.as_deref()) {
            (Some(template), Some(body)) => Some(template.against(body)),
            _ => None,
        },
    }
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
    for problem in &report.problems {
        let _ = write!(lines, "\n  PROBLEM: {problem}");
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

/// One branch row, every fact as a fixed token so a reader can scan a column.
fn render_branch(row: &BranchFacts) -> String {
    let origin = match row.tip_matches_origin {
        Some(true) => "same",
        Some(false) => "differs",
        None => "absent",
    };
    let mut line = format!(
        "{}  tip {}  origin {origin}",
        row.branch,
        short_id(&row.tip)
    );
    if row.fork_only {
        line.push_str("  fork-only");
    }
    match &row.pull {
        Some(pull) => {
            let _ = write!(
                line,
                "  pr #{} {} mergeable={} state={} review={} head={}",
                pull.number,
                pull.state,
                pull.mergeable.as_deref().unwrap_or("-"),
                pull.merge_state_status.as_deref().unwrap_or("-"),
                if pull.review_decision.is_empty() {
                    "-"
                } else {
                    pull.review_decision.as_str()
                },
                if pull.head_matches_tip {
                    "matches"
                } else {
                    "differs"
                }
            );
            match &pull.checks {
                Some(checks) => {
                    let conclusions = checks
                        .conclusions
                        .iter()
                        .map(|(conclusion, count)| format!("{conclusion} {count}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = write!(
                        line,
                        "  checks {} ({conclusions}; {} pending)",
                        checks.total, checks.pending
                    );
                }
                None => line.push_str("  checks -"),
            }
            match pull.unresolved_review_threads {
                Some(count) => {
                    let _ = write!(line, "  threads {count} unresolved");
                }
                None => line.push_str("  threads -"),
            }
            match &pull.template {
                Some(template) if template.missing_from_body.is_empty() => {
                    line.push_str("  template ok");
                }
                Some(template) => {
                    let _ = write!(
                        line,
                        "  template missing: {}",
                        template.missing_from_body.join(", ")
                    );
                }
                None => line.push_str("  template -"),
            }
        }
        None => line.push_str("  no-pr  checks -  threads -  template -"),
    }
    match &row.forbidden {
        Some(hits) if hits.is_empty() => line.push_str("  forbidden none"),
        Some(hits) => {
            let places = hits
                .iter()
                .map(|hit| format!("{}:{} {}", hit.file, hit.line, hit.term))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = write!(line, "  forbidden {} hits: {places}", hits.len());
        }
        None => line.push_str("  forbidden -"),
    }
    line
}

#[cfg(test)]
mod tests {
    use super::{
        AuditInput, LocalFacts, PullHeadInput, ReleaseDriftScan, Report,
        add_misplaced_origin_release_refs, add_open_pull_head_checks, add_release_drifts, exit_for,
        pull_head_findings, pull_position_findings, recorded_commit, same_commit,
    };
    use crate::bind::Fork;
    use crate::cli::Exit;
    use crate::config::RepoEntry;
    use crate::detect::{FindingKind, Subject};
    use crate::forge::{
        Account, Forge, ForgeError, PullDetails, PullFacts, PullRequest, PullSummary, RepoIdentity,
        SweepPage, TimelineEvent, fake::FakeForge,
    };
    use crate::ids::{BookmarkRef, BranchName, CommitId, ReleaseScheme, RepoName};
    use crate::ledger::{Entry, Kind, Ledger};
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
            scheme: &ReleaseScheme::Dated,
            tracked: &NO_TRACKED,
            template: None,
        }
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
                subject: Some("release/2026-08-15".to_owned()),
                kind: Kind::Event,
                disposition: None,
                text: "cut release/2026-08-15 as aaaaaaaaaaaa with 1 parent(s)".to_owned(),
                evidence: vec!["aaaaaaaaaaaa".to_owned(), "member0000000".to_owned()],
                anchor: None,
                pr: None,
                parents: Vec::new(),
            },
            Entry {
                ts: "2026-08-16T00:00:00Z".to_owned(),
                owner: "test".to_owned(),
                subject: Some("release/2026-08-15".to_owned()),
                kind: Kind::Note,
                disposition: Some("ruled-out".to_owned()),
                text: "later disposition".to_owned(),
                evidence: vec!["bbbbbbbbbbbb".to_owned()],
                anchor: None,
                pr: None,
                parents: Vec::new(),
            },
        ];

        assert_eq!(
            recorded_commit(&entries, "release/2026-08-15"),
            Some(CommitId::new("aaaaaaaaaaaa"))
        );
    }

    #[test]
    fn a_publish_only_release_drift_is_compared_to_its_recorded_cut() {
        let directory = tempfile::tempdir().expect("ledger directory");
        let ledger = Ledger::at(directory.path().to_owned());
        ledger
            .append(&Entry {
                ts: "2026-08-15T00:00:00Z".to_owned(),
                owner: "test".to_owned(),
                subject: Some("release/2026-08-15".to_owned()),
                kind: Kind::Event,
                disposition: None,
                text: "cut release/2026-08-15 as aaaaaaaaaaaa with 1 parent(s)".to_owned(),
                evidence: vec!["aaaaaaaaaaaa".to_owned(), "bbbbbbbbbbbb".to_owned()],
                anchor: None,
                pr: None,
                parents: Vec::new(),
            })
            .expect("record cut");
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };
        let published = BTreeMap::from([(
            "refs/heads/release/2026-08-15".to_owned(),
            CommitId::new("cccccccccccccccccccccccccccccccccccccccc"),
        )]);

        add_release_drifts(
            &mut report,
            &ReleaseDriftScan {
                local: &BTreeMap::new(),
                published: &published,
                scheme: &crate::ids::ReleaseScheme::Dated,
                publish_remote: "release",
                ledger: &ledger,
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
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };
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
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

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
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        pull_head_findings(
            &PullHeadInput {
                fork: &fork,
                forge: &forge,
                cache_root: None,
                local: &local,
                origin_refs: &origin,
                tracked: &BTreeMap::new(),
                template: None,
            },
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
        let store = Store::open(temp.path().join("state.json")).expect("open test store");
        let repo = RepoName::new("demo");
        let mut report = Report {
            repo: repo.to_string(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        add_open_pull_head_checks(
            &mut report,
            &AuditInput {
                fork: &fork,
                store: &store,
                forge: Some(&forge),
                cache_root: Some(temp.path()),
            },
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
        let store = Store::open(temp.path().join("state.json")).expect("open test store");
        let repo = RepoName::new("demo");
        let mut report = Report {
            repo: repo.to_string(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        add_open_pull_head_checks(
            &mut report,
            &AuditInput {
                fork: &fork,
                store: &store,
                forge: Some(&forge),
                cache_root: Some(&blocked_root),
            },
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
        let store = Store::open(temp.path().join("state.json")).expect("open test store");
        let repo = RepoName::new("demo");
        let local = BTreeMap::new();
        let origin = BTreeMap::new();
        let mut report = Report {
            repo: repo.to_string(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        add_open_pull_head_checks(
            &mut report,
            &AuditInput {
                fork: &fork,
                store: &store,
                forge: Some(&forge),
                cache_root: None,
            },
            &LocalFacts {
                local: &local,
                origin_refs: &origin,
                scheme: &ReleaseScheme::Dated,
                tracked: &NO_TRACKED,
                template: None,
            },
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
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: Vec::new(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        pull_head_findings(
            &PullHeadInput {
                fork: &fork,
                forge: &forge,
                cache_root: None,
                local: &local,
                origin_refs: &origin,
                tracked: &BTreeMap::new(),
                template: None,
            },
            &mut report,
        )
        .expect("the changed pull fact is still a successful batch answer");

        assert!(report.findings.is_empty(), "was: {:?}", report.findings);
    }

    /// A row for `branch` at `tip`, the way `add_branch_facts` shapes one.
    fn local_row(branch: &str, tip: &str) -> super::BranchFacts {
        super::BranchFacts {
            branch: branch.to_owned(),
            tip: tip.to_owned(),
            origin_tip: None,
            tip_matches_origin: None,
            fork_only: false,
            pull: None,
            forbidden: None,
        }
    }

    #[test]
    fn a_tracked_pull_of_another_author_fills_the_row_without_moving_the_exit() {
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
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: vec![local_row("feat/alpha", tip)],
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        pull_head_findings(
            &PullHeadInput {
                fork: &fork,
                forge: &forge,
                cache_root: None,
                local: &local,
                origin_refs: &BTreeMap::new(),
                tracked: &tracked,
                template: None,
            },
            &mut report,
        )
        .expect("the tracked number is answered");

        // Then: the row carries the pull's facts, and nothing else moves — their
        // branch name is not ours to reconcile against origin or a bookmark.
        let pull = report
            .branches
            .first()
            .expect("one row")
            .pull
            .as_ref()
            .expect("tracked pull facts");
        assert_eq!((pull.number, pull.head_matches_tip), (41, true));
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
        let tip = "local000000000000000000000000000000000000";
        let local = BTreeMap::from([(BranchName::new("feat/alpha"), CommitId::new(tip))]);
        let tracked = BTreeMap::from([(BranchName::new("feat/alpha"), 41)]);
        let mut report = Report {
            repo: "demo".to_owned(),
            branches: vec![local_row("feat/alpha", tip)],
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        pull_head_findings(
            &PullHeadInput {
                fork: &fork,
                forge: &forge,
                cache_root: None,
                local: &local,
                origin_refs: &BTreeMap::new(),
                tracked: &tracked,
                template: None,
            },
            &mut report,
        )
        .expect("an unanswered tracked number is not a batch failure");

        assert!(report.branches.first().expect("one row").pull.is_none());
        assert_eq!(
            report.notes,
            vec!["tracked pull request #41 was not answered by the live batch"]
        );
        assert!(report.problems.is_empty(), "was: {:?}", report.problems);
        assert_eq!(exit_for(&report), Exit::Ok);
    }
}
