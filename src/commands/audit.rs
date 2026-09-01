//! `knives audit`: reconcile the fork estate against live refs and recorded facts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::cli::Exit;
use crate::commands::pushed::{self, ReconcileInput, Row};
use crate::config::{RepoEntry, Role};
use crate::detect::{Finding, FindingKind, Subject};
use crate::forge::{Forge, PullRequest};
use crate::ids::{BookmarkRef, BranchName, BranchTarget, CommitId, RepoName, is_release_name};
use crate::jj::{self, Repo};
use crate::ledger::{Entry, Ledger};
use crate::snapshot::{self, SnapshotConfig};
use crate::store::Store;

const ORPHAN_REVSET: &str = r#"heads(all()) ~ ::(bookmarks() | remote_bookmarks() | tags()) ~ working_copies() ~ (empty() & description(exact:""))"#;

/// Every audit finding, note, and unanswered check for one repository.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

/// Dependencies shared by the read-only estate checks.
pub struct AuditInput<'a> {
    pub repo: &'a RepoName,
    pub entry: &'a RepoEntry,
    pub store: &'a Store,
    pub forge: Option<&'a dyn Forge>,
    pub cache_root: Option<&'a Path>,
}

impl std::fmt::Debug for AuditInput<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditInput")
            .field("repo", self.repo)
            .field("entry", self.entry)
            .field("forge", &self.forge.is_some())
            .field("cache_root", &self.cache_root)
            .finish()
    }
}

/// Gather the estate facts without writing a repository, remote, store, or ledger.
pub fn gather(input: &AuditInput<'_>) -> Report {
    let mut report = Report {
        repo: input.repo.to_string(),
        findings: Vec::new(),
        notes: Vec::new(),
        problems: Vec::new(),
    };
    let opened = match Repo::open(&input.entry.path) {
        Ok(opened) => opened,
        Err(error) => {
            report.problems.push(format!(
                "could not open {}: {error}",
                input.entry.path.display()
            ));
            return report;
        }
    };
    let local = match opened.bookmark_tips() {
        Ok(tips) => pushed::local_tips(tips),
        Err(error) => {
            report
                .problems
                .push(format!("could not read local bookmarks: {error}"));
            return report;
        }
    };
    let live = match pushed::live_refs(input.entry) {
        Ok(live) => live,
        Err(error) => {
            report
                .problems
                .push(format!("could not read live push refs: {error}"));
            return report;
        }
    };
    let scheme = input.entry.release_scheme();
    let requested: Vec<BranchName> = local.keys().cloned().collect();
    let tracked = tracked(input.store, input.repo, &requested);
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
        entry: input.entry,
        store: input.store,
        repo: input.repo,
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
            publish_remote: input.entry.publish_remote(),
            ledger: &Ledger::for_repo(input.repo),
        },
    );
    add_misplaced_origin_release_refs(
        &mut report,
        live.origin(),
        &scheme,
        input.entry.publish_remote(),
    );
    add_orphan_commits(&mut report, &opened, &input.entry.path);
    add_open_pull_head_checks(&mut report, input, &local, live.origin());
    report
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
                            short(commit.as_str())
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
                        short(commit.as_str())
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
                short(current.as_str()),
                short(recorded.as_str()),
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
                short(commit.as_str())
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

fn add_open_pull_head_checks(
    report: &mut Report,
    input: &AuditInput<'_>,
    local: &BTreeMap<BranchName, CommitId>,
    origin_refs: &BTreeMap<String, CommitId>,
) {
    let Some(forge) = input.forge else {
        report
            .problems
            .push("open pull-head reconciliation was skipped (--no-github)".to_owned());
        return;
    };
    let request = PullHeadInput {
        entry: input.entry,
        forge,
        cache_root: input.cache_root,
        local,
        origin_refs,
    };
    match pull_head_findings(&request, &mut report.findings, &mut report.notes) {
        Ok(problems) => report.problems.extend(problems),
        Err(error) => report
            .problems
            .push(format!("could not read open pull-request heads: {error}")),
    }
}

struct PullHeadInput<'a> {
    entry: &'a RepoEntry,
    forge: &'a dyn Forge,
    cache_root: Option<&'a Path>,
    local: &'a BTreeMap<BranchName, CommitId>,
    origin_refs: &'a BTreeMap<String, CommitId>,
}

fn pull_head_findings(
    input: &PullHeadInput<'_>,
    findings: &mut Vec<Finding>,
    notes: &mut Vec<String>,
) -> Result<Vec<String>, crate::forge::ForgeError> {
    let opened = snapshot::open(SnapshotConfig {
        forge: input.forge,
        path: &input.entry.path,
        remotes: [
            input.entry.remote(Role::Origin),
            input.entry.remote(Role::Release),
        ],
        cache_root: input.cache_root,
    })?;
    let snapshot = opened.complete_with(&(), |discovery, ()| {
        discovery
            .ours()
            .iter()
            .filter(|pull| pull.is_open())
            .map(|pull| pull.number)
            .collect()
    })?;
    let mut problems = Vec::new();
    for number in snapshot.requested() {
        match snapshot.fact(*number) {
            Some(fact) if fact.pull.is_open() => {
                pull_position_findings(&fact.pull, input.local, input.origin_refs, findings);
            }
            Some(_) => {}
            None => problems.push(format!(
                "open pull request #{number} was not answered by the live batch"
            )),
        }
    }
    if let Err(note) = snapshot.persist(None) {
        notes.push(note.to_string());
    }
    Ok(problems)
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
            short(&pull.head_ref_oid),
            short(actual)
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

/// Render findings grouped by detector, followed by the one deeper-history surface.
pub fn render(report: &Report) -> String {
    let mut lines = format!("{}: audit", report.repo);
    for problem in &report.problems {
        let _ = write!(lines, "\n  PROBLEM: {problem}");
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

fn short(value: &str) -> String {
    value.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        AuditInput, PullHeadInput, ReleaseDriftScan, Report, add_misplaced_origin_release_refs,
        add_open_pull_head_checks, add_release_drifts, exit_for, pull_head_findings,
        pull_position_findings, recorded_commit, same_commit,
    };
    use crate::cli::Exit;
    use crate::config::RepoEntry;
    use crate::detect::{FindingKind, Subject};
    use crate::forge::{
        Account, ConsumerHead, Forge, ForgeError, PullDetails, PullFacts, PullRequest, PullSummary,
        RepoIdentity, SweepPage, TimelineEvent, fake::FakeForge,
    };
    use crate::ids::{BookmarkRef, BranchName, CommitId, RepoName};
    use crate::ledger::{Entry, Kind, Ledger};
    use crate::store::Store;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

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

        fn consumer_head(&self, _repo: &Path, _slug: &str) -> Result<ConsumerHead, ForgeError> {
            Err(ForgeError::Query {
                detail: "consumer lookups are not part of this test".to_owned(),
            })
        }

        fn file_at(
            &self,
            _repo: &Path,
            _slug: &str,
            _commit: &str,
            _path: &str,
        ) -> Result<Option<String>, ForgeError> {
            Err(ForgeError::Query {
                detail: "consumer lookups are not part of this test".to_owned(),
            })
        }
    }

    fn test_entry() -> RepoEntry {
        RepoEntry {
            path: PathBuf::from("/fake"),
            upstream: "git@github.com:upstream/repo.git".to_owned(),
            origin: "git@github.com:owner/fork.git".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        }
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
            })
            .expect("record cut");
        let mut report = Report {
            repo: "demo".to_owned(),
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
            path: PathBuf::from("/tmp/demo"),
            upstream: "https://forge.invalid/up/demo.git".to_owned(),
            origin: "https://forge.invalid/ours/demo.git".to_owned(),
            base: None,
            release: Some("https://forge.invalid/ours/demo.git".to_owned()),
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        };
        let mut report = Report {
            repo: "demo".to_owned(),
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
        let entry = RepoEntry {
            path: PathBuf::from("/fake"),
            upstream: "git@github.com:upstream/repo.git".to_owned(),
            origin: "git@github.com:owner/fork.git".to_owned(),
            base: None,
            release: None,
            release_branch: None,
            test_count_command: None,
            consumers: Vec::new(),
        };
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
        let mut findings = Vec::new();
        let mut notes = Vec::new();

        pull_head_findings(
            &PullHeadInput {
                entry: &entry,
                forge: &forge,
                cache_root: None,
                local: &local,
                origin_refs: &origin,
            },
            &mut findings,
            &mut notes,
        )
        .expect("fake forge should complete the open pull batch");

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
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(BranchName::new("feat/alpha"), test_pull("OPEN"))]),
            ..FakeForge::default()
        };
        let temp = tempfile::tempdir().expect("create test cache");
        let store = Store::open(temp.path().join("state.json")).expect("open test store");
        let repo = RepoName::new("demo");
        let mut report = Report {
            repo: repo.to_string(),
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        add_open_pull_head_checks(
            &mut report,
            &AuditInput {
                repo: &repo,
                entry: &entry,
                store: &store,
                forge: Some(&forge),
                cache_root: Some(temp.path()),
            },
            &BTreeMap::new(),
            &BTreeMap::new(),
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
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        add_open_pull_head_checks(
            &mut report,
            &AuditInput {
                repo: &repo,
                entry: &entry,
                store: &store,
                forge: Some(&forge),
                cache_root: Some(&blocked_root),
            },
            &BTreeMap::new(),
            &BTreeMap::new(),
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
            findings: Vec::new(),
            notes: Vec::new(),
            problems: Vec::new(),
        };

        add_open_pull_head_checks(
            &mut report,
            &AuditInput {
                repo: &repo,
                entry: &entry,
                store: &store,
                forge: Some(&forge),
                cache_root: None,
            },
            &local,
            &origin,
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
        let mut findings = Vec::new();
        let mut notes = Vec::new();

        pull_head_findings(
            &PullHeadInput {
                entry: &entry,
                forge: &forge,
                cache_root: None,
                local: &local,
                origin_refs: &origin,
            },
            &mut findings,
            &mut notes,
        )
        .expect("the changed pull fact is still a successful batch answer");

        assert!(findings.is_empty(), "was: {findings:?}");
    }
}
