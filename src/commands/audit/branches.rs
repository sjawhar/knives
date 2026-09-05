//! The audit's per-branch row: its facts, how they are gathered from the
//! checkout, the store and the live batch, and how one row renders.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::config::RepoEntry;
use crate::forge::ChecksSummary;
use crate::ids::{BookmarkRef, BranchName, BranchTarget, CommitId, ReleaseScheme, is_release_name};
use crate::jj::{self, Repo};
use crate::snapshot::CompletedSnapshot;

use super::{AuditInput, Report};

/// Facts about one maintained branch and its pull request. Facts, never
/// verdicts: each field is an observation, and `None` means unobserved.
#[derive(Debug, serde::Serialize)]
pub struct BranchFacts {
    pub branch: BranchName,
    pub tip: CommitId,
    /// Where origin holds the branch; `None` when origin has no such ref.
    pub origin_tip: Option<CommitId>,
    /// `origin_tip == tip`; `None` when origin has no such ref.
    tip_matches_origin: Option<bool>,
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

impl BranchFacts {
    /// The row's local facts, before any forge, release or scan is consulted.
    pub fn local(
        branch: BranchName,
        tip: CommitId,
        origin_tip: Option<CommitId>,
        fork_only: bool,
    ) -> Self {
        Self {
            tip_matches_origin: origin_tip.as_ref().map(|origin| *origin == tip),
            branch,
            tip,
            origin_tip,
            fork_only,
            pull: None,
            forbidden: None,
        }
    }

    /// Whether origin holds the branch at the local tip; `None` when origin
    /// has no such ref.
    pub const fn tip_matches_origin(&self) -> Option<bool> {
        self.tip_matches_origin
    }
}

/// The open pull request on a maintained branch, as the live batch answered it.
#[derive(Debug, serde::Serialize)]
pub struct PullSnapshot {
    pub number: u64,
    pub url: String,
    pub head: String,
    pub head_matches_tip: bool,
    pub mergeable: Option<String>,
    pub merge_state_status: Option<String>,
    /// `None` when the forge reports no review decision.
    pub review_decision: Option<String>,
    pub checks: Option<CheckCounts>,
    pub unresolved_review_threads: Option<usize>,
    /// The report's template headings the body carries no heading for; `None`
    /// when upstream's trunk has no pull-request template or the batch did not
    /// answer the body.
    pub template_missing: Option<Vec<String>>,
}

/// Check runs on the pull request's head, counted by conclusion.
#[derive(Debug, serde::Serialize)]
pub struct CheckCounts {
    pub total: usize,
    pub pending: usize,
    pub conclusions: BTreeMap<String, usize>,
}

impl CheckCounts {
    /// Count `runs`: every run once in `total`, the unfinished ones in
    /// `pending`, and the finished ones under their upper-cased conclusion.
    fn from_runs(checks: &ChecksSummary) -> Self {
        let mut conclusions: BTreeMap<String, usize> = BTreeMap::new();
        let mut pending = 0;
        for run in &checks.runs {
            match &run.conclusion {
                Some(conclusion) => *conclusions.entry(conclusion.to_uppercase()).or_default() += 1,
                None => pending += 1,
            }
        }
        Self {
            total: checks.runs.len(),
            pending,
            conclusions,
        }
    }
}

/// Upstream's pull-request template, read once per run.
#[derive(Debug, serde::Serialize)]
pub struct Template {
    pub file: String,
    pub headings: Vec<String>,
}

impl Template {
    /// The headings `body` does not carry: a body heading whose text equals
    /// the heading case-insensitively carries it.
    fn missing_from(&self, body: &str) -> Vec<String> {
        let carried: Vec<String> = headings(body).map(str::to_lowercase).collect();
        self.headings
            .iter()
            .filter(|heading| !carried.contains(&heading.to_lowercase()))
            .cloned()
            .collect()
    }
}

/// The Markdown ATX headings of `text`, in order, skipping lines inside an
/// HTML comment (`<!-- … -->`, the guidance templates carry) or a fenced code
/// block (```` ``` ````, where a `#` line is code or a shell comment). A
/// heading followed by a comment on the same line is still a heading.
fn headings(text: &str) -> impl Iterator<Item = &str> {
    let mut in_comment = false;
    let mut in_fence = false;
    text.lines().filter_map(move |line| {
        if in_comment {
            in_comment = !line.contains("-->");
            return None;
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            return None;
        }
        if in_fence {
            return None;
        }
        let Some(opened) = line.find("<!--") else {
            return heading_text(line);
        };
        in_comment = !line.get(opened..).is_some_and(|rest| rest.contains("-->"));
        heading_text(line.get(..opened)?.trim_end())
    })
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

/// The first pull-request template among the spellings upstream's trunk
/// carries, or `None` when it carries none.
pub(super) fn read_template(
    path: &Path,
    entry: &RepoEntry,
) -> Result<Option<Template>, jj::JjError> {
    let trunk = entry.upstream_trunk();
    for file in &crate::commands::preflight::PULL_REQUEST_TEMPLATES {
        if let Some(text) = jj::file_text(path, &trunk, file)? {
            return Ok(Some(Template {
                file: (*file).to_owned(),
                headings: headings(&text).map(str::to_owned).collect(),
            }));
        }
    }
    Ok(None)
}

/// What the checkout, the live origin refs and the store say, held against
/// the forge's answers.
pub(super) struct LocalFacts<'a> {
    pub(super) local: &'a BTreeMap<BranchName, CommitId>,
    pub(super) origin_refs: &'a BTreeMap<String, CommitId>,
    /// Pull numbers the store tracks per branch, asked for beside the open
    /// ones: a tracked pull request another owner submitted is not discovered
    /// as ours, and its head is still the head to compare.
    pub(super) tracked: &'a BTreeMap<BranchName, u64>,
    /// Upstream's pull-request template, when its trunk carries one.
    pub(super) template: Option<&'a Template>,
}

/// The bookmarks the rows are built from.
pub(super) struct RowInput<'a> {
    /// The live origin refs, `refs/heads/<branch>` to commit.
    pub(super) origin_refs: &'a BTreeMap<String, CommitId>,
    /// Every local bookmark that is neither the trunk nor a release, with its
    /// tip, in bookmark order: the branches that get a row.
    pub(super) carried: &'a [(String, CommitId)],
    /// Every conflicted bookmark with the commits it points at.
    pub(super) conflicted: &'a [(BookmarkRef, Vec<CommitId>)],
}

/// One row per carried branch with its local facts and, when configured and
/// not exempt, the forbidden-identifier scan of the lines it adds over its fork
/// point with upstream's trunk. A divergent (conflicted) local bookmark has no
/// single tip and gets a `problems` line instead of a row. A trunk the checkout
/// cannot resolve (never fetched) is one problem line, not one per branch, and
/// leaves every row's `forbidden` absent.
pub(super) fn add_branch_facts(
    report: &mut Report,
    input: &AuditInput<'_>,
    opened: &Repo,
    rows: &RowInput<'_>,
) {
    let fork = input.fork;
    let entry = fork.entry;
    add_divergent_problems(
        report,
        rows.conflicted,
        entry.trunk(),
        &entry.release_scheme(),
    );
    let mut built: Vec<BranchFacts> = rows
        .carried
        .iter()
        .map(|(branch, tip)| {
            let branch = BranchName::new(branch);
            BranchFacts::local(
                branch.clone(),
                tip.clone(),
                rows.origin_refs
                    .get(&format!("refs/heads/{branch}"))
                    .cloned(),
                input
                    .store
                    .is_fork_only(&BranchTarget::new(fork.name.clone(), branch)),
            )
        })
        .collect();
    if !entry.forbidden.is_empty() {
        add_forbidden_scans(report, input, opened, &mut built);
    }
    report.branches.extend(built);
}

/// One problem line per conflicted local bookmark: a branch that would
/// otherwise get a row has no single tip, so it has no row; a release has no
/// single tip either, so its drift from the recorded cut goes unread. The trunk
/// is `knives status`'s `divergence` finding and not repeated here.
fn add_divergent_problems(
    report: &mut Report,
    conflicted: &[(BookmarkRef, Vec<CommitId>)],
    trunk: &str,
    scheme: &ReleaseScheme,
) {
    for (reference, targets) in conflicted {
        let BookmarkRef::Local(branch) = reference else {
            continue;
        };
        if branch.as_str() == trunk {
            continue;
        }
        let targets = targets.len();
        report.problems.push(if is_release_name(branch, scheme) {
            format!("release {branch} is divergent ({targets} targets)")
        } else {
            format!("bookmark {branch} is divergent ({targets} targets); no row")
        });
    }
}

/// Scan every row that is not fork-only, once upstream's trunk resolves.
fn add_forbidden_scans(
    report: &mut Report,
    input: &AuditInput<'_>,
    opened: &Repo,
    rows: &mut [BranchFacts],
) {
    let fork = input.fork;
    let entry = fork.entry;
    let upstream_trunk = entry.upstream_trunk();
    if let Err(error) = opened.resolve_commit(&upstream_trunk) {
        report.problems.push(format!(
            "upstream trunk {upstream_trunk} cannot be resolved; forbidden scans skipped: {error}"
        ));
        return;
    }
    let scanned: Vec<&str> = rows
        .iter()
        .filter(|row| !row.fork_only)
        .map(|row| row.branch.as_str())
        .collect();
    let mut results = forbidden_scans(&fork.checkout.path, entry, &scanned, input.workers);
    for row in rows {
        match results.remove(row.branch.as_str()) {
            Some(Ok(hits)) => row.forbidden = Some(hits),
            Some(Err(problem)) => report.problems.push(problem),
            None => {}
        }
    }
}

/// The most `jj diff` subprocesses one audit runs at once. The scan is bound by
/// the subprocess and the disk, not the CPU, and several owners audit the same
/// checkout in the same minute: a machine's full width per audit would put
/// hundreds of `jj` processes on one repository.
const MAX_SCAN_WORKERS: usize = 8;

/// The forbidden-identifier scan of every named branch's diff from its fork
/// point with upstream's trunk — so upstream's own newer lines are never the
/// branch's additions — one `jj diff` subprocess per branch across at most
/// `workers` threads (capped by [`MAX_SCAN_WORKERS`]): fifty branches at two
/// hundred milliseconds each is ten seconds serial. An unreadable diff is a
/// problem line naming the branch.
fn forbidden_scans(
    path: &Path,
    entry: &RepoEntry,
    branches: &[&str],
    workers: usize,
) -> BTreeMap<String, Result<Vec<crate::forbidden::Hit>, String>> {
    if branches.is_empty() {
        return BTreeMap::new();
    }
    let upstream_trunk = entry.upstream_trunk();
    let upstream_trunk = upstream_trunk.as_str();
    let workers = workers.min(MAX_SCAN_WORKERS).clamp(1, branches.len());
    let chunk = branches.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for slice in branches.chunks(chunk) {
            handles.push((
                slice,
                scope.spawn(move || {
                    slice
                        .iter()
                        .map(|branch| {
                            let from = format!("fork_point({upstream_trunk} | {branch})");
                            let scan = jj::diff_git(path, &from, branch)
                                .map_err(|error| error.to_string())
                                .and_then(|diff| crate::forbidden::scan(&diff, &entry.forbidden))
                                .map_err(|error| {
                                    format!(
                                        "could not scan {branch} from {from} for forbidden identifiers: {error}"
                                    )
                                });
                            ((*branch).to_owned(), scan)
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
                        .map(|branch| {
                            (
                                (*branch).to_owned(),
                                Err(format!("forbidden scan task panicked for {branch}")),
                            )
                        })
                        .collect()
                })
            })
            .collect()
    })
}

/// A [`PullSnapshot`] on every row whose primary pull request the batch
/// answered open: the branch's primary by the crate's one rule (open beats
/// closed), with a `tracked` number standing in when discovery listed none.
pub(super) fn attach_pulls(
    rows: &mut [BranchFacts],
    snapshot: &CompletedSnapshot<'_>,
    tracked: &BTreeMap<BranchName, u64>,
    template: Option<&Template>,
) {
    for row in rows {
        let number = snapshot
            .index()
            .by_branch
            .get(&row.branch)
            .filter(|primary| primary.is_open())
            .map(|primary| primary.number)
            .or_else(|| tracked.get(&row.branch).copied());
        row.pull = number
            .and_then(|number| snapshot.fact(number))
            .filter(|fact| fact.pull.is_open())
            .map(|fact| pull_snapshot(fact, &row.tip, template));
    }
}

/// The pull-request facts the live batch answered, counted and held against
/// the branch tip and upstream's template.
fn pull_snapshot(
    fact: &crate::forge::PullFacts,
    tip: &CommitId,
    template: Option<&Template>,
) -> PullSnapshot {
    let pull = &fact.pull;
    let details = &fact.details;
    PullSnapshot {
        number: pull.number,
        url: pull.url.clone(),
        head: pull.head_ref_oid.clone(),
        head_matches_tip: pull.head_ref_oid == tip.as_str(),
        mergeable: pull.mergeable.clone(),
        merge_state_status: pull.merge_state_status.clone(),
        review_decision: (!pull.review_decision.is_empty()).then(|| pull.review_decision.clone()),
        checks: details.checks.as_ref().map(CheckCounts::from_runs),
        unresolved_review_threads: details.unresolved_review_threads,
        template_missing: match (template, details.body.as_deref()) {
            (Some(template), Some(body)) => Some(template.missing_from(body)),
            _ => None,
        },
    }
}

/// One branch row, every fact as a fixed token so a reader can scan a column;
/// `-` is an unanswered fact.
pub(super) fn render_branch(row: &BranchFacts) -> String {
    let origin = match row.tip_matches_origin {
        Some(true) => "same",
        Some(false) => "differs",
        None => "absent",
    };
    let mut line = format!("{}  tip {}  origin {origin}", row.branch, row.tip.short());
    if row.fork_only {
        line.push_str("  fork-only");
    }
    let pull = row.pull.as_ref();
    match pull {
        Some(pull) => {
            let _ = write!(
                line,
                "  pr #{} mergeable={} merge_state={} review={} head={}",
                pull.number,
                pull.mergeable.as_deref().unwrap_or("-"),
                pull.merge_state_status.as_deref().unwrap_or("-"),
                pull.review_decision.as_deref().unwrap_or("-"),
                if pull.head_matches_tip {
                    "matches"
                } else {
                    "differs"
                }
            );
        }
        None => line.push_str("  no-pr"),
    }
    push_checks(&mut line, pull.and_then(|pull| pull.checks.as_ref()));
    push_threads(
        &mut line,
        pull.and_then(|pull| pull.unresolved_review_threads),
    );
    push_template(
        &mut line,
        pull.and_then(|pull| pull.template_missing.as_deref()),
    );
    push_forbidden(&mut line, row.forbidden.as_deref());
    line
}

fn push_checks(line: &mut String, checks: Option<&CheckCounts>) {
    match checks {
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
}

fn push_threads(line: &mut String, unresolved: Option<usize>) {
    match unresolved {
        Some(count) => {
            let _ = write!(line, "  threads {count} unresolved");
        }
        None => line.push_str("  threads -"),
    }
}

fn push_template(line: &mut String, missing: Option<&[String]>) {
    match missing {
        Some([]) => line.push_str("  template none missing"),
        Some(missing) => {
            let _ = write!(line, "  template missing: {}", missing.join(", "));
        }
        None => line.push_str("  template -"),
    }
}

fn push_forbidden(line: &mut String, hits: Option<&[crate::forbidden::Hit]>) {
    match hits {
        Some([]) => line.push_str("  forbidden none"),
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
}

#[cfg(test)]
mod tests {
    use super::{
        BranchFacts, CheckCounts, PullSnapshot, Template, attach_pulls, headings, render_branch,
    };
    use crate::forge::{Account, CheckRun, ChecksSummary, Forge, PullRequest, fake::FakeForge};
    use crate::ids::{BranchName, CommitId};
    use crate::snapshot::{self, CompletedSnapshot, Opened, SnapshotConfig};
    use std::collections::BTreeMap;
    use std::path::Path;

    /// The fork remote whose owner makes a pull request ours: the ssh form, so
    /// the identity guard's forge-URL needle does not match.
    const ORIGIN: &str = "git@github.com:owner/fork.git";

    static NO_TRACKED: BTreeMap<BranchName, u64> = BTreeMap::new();

    /// A row for `branch` at `tip`, the way `add_branch_facts` shapes one.
    fn local_row(branch: &str, tip: &str) -> BranchFacts {
        BranchFacts::local(BranchName::new(branch), CommitId::new(tip), None, false)
    }

    /// An open pull request `number` from `owner`'s fork, head `branch` at `head`.
    fn open_pull(number: u64, owner: &str, branch: &str, head: &str) -> PullRequest {
        PullRequest {
            number,
            state: "OPEN".to_owned(),
            head_ref_name: branch.to_owned(),
            head_ref_oid: head.to_owned(),
            head_repository_owner: Some(Account {
                login: owner.to_owned(),
            }),
            ..PullRequest::default()
        }
    }

    /// The forge opened for the fork at [`ORIGIN`], with no cache.
    fn opened(forge: &dyn Forge) -> Opened<'_> {
        snapshot::open(SnapshotConfig {
            forge,
            path: Path::new("/fake"),
            remotes: [ORIGIN, ORIGIN],
            cache_root: None,
        })
        .expect("the fake forge answers its identity")
    }

    /// One live batch over every pull request of ours and every `tracked` number.
    fn batch<'o>(
        opened: &'o Opened<'o>,
        tracked: &BTreeMap<BranchName, u64>,
    ) -> CompletedSnapshot<'o> {
        opened
            .complete_with(tracked, |discovery, tracked| {
                discovery
                    .ours()
                    .iter()
                    .map(|pull| pull.number)
                    .chain(tracked.values().copied())
                    .collect()
            })
            .expect("the fake forge answers the batch")
    }

    #[test]
    fn origin_parity_is_computed_from_the_two_tips() {
        let tip = CommitId::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let other = CommitId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let same = BranchFacts::local(BranchName::new("a"), tip.clone(), Some(tip.clone()), false);
        let differs = BranchFacts::local(BranchName::new("a"), tip.clone(), Some(other), false);
        let absent = BranchFacts::local(BranchName::new("a"), tip, None, false);

        assert_eq!(same.tip_matches_origin(), Some(true));
        assert_eq!(differs.tip_matches_origin(), Some(false));
        assert_eq!(absent.tip_matches_origin(), None);
    }

    #[test]
    fn a_row_serialises_forbidden_as_an_empty_list_or_not_at_all() {
        // Given: one scanned row with no hits, one row never scanned.
        let tip = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut scanned = local_row("feat/a", tip);
        scanned.forbidden = Some(Vec::new());
        let unscanned = local_row("feat/b", tip);

        // When: both are serialised the way `--json` emits them.
        let scanned = serde_json::to_value(&scanned).expect("serialise");
        let unscanned = serde_json::to_value(&unscanned).expect("serialise");

        // Then: `forbidden: []` says "scanned, nothing found"; absent says "not scanned";
        // the newtypes serialise as the plain strings they were.
        assert_eq!(scanned.get("forbidden"), Some(&serde_json::json!([])));
        assert!(unscanned.get("forbidden").is_none(), "was: {unscanned}");
        assert!(unscanned.get("pull").is_none(), "was: {unscanned}");
        assert_eq!(
            unscanned,
            serde_json::json!({
                "branch": "feat/b",
                "tip": tip,
                "origin_tip": null,
                "tip_matches_origin": null,
                "fork_only": false,
            })
        );
    }

    #[test]
    fn check_counts_split_pending_from_concluded_runs() {
        let counts = CheckCounts::from_runs(&ChecksSummary {
            runs: vec![
                CheckRun {
                    name: "lint".to_owned(),
                    conclusion: Some("success".to_owned()),
                },
                CheckRun {
                    name: "test".to_owned(),
                    conclusion: None,
                },
                CheckRun {
                    name: "e2e".to_owned(),
                    conclusion: Some("SUCCESS".to_owned()),
                },
            ],
        });

        assert_eq!((counts.total, counts.pending), (3, 1));
        assert_eq!(
            counts.conclusions,
            BTreeMap::from([("SUCCESS".to_owned(), 2)])
        );
    }

    #[test]
    fn a_branch_row_renders_every_fact_as_a_fixed_token() {
        // Given: a fork-only row without a pull and a row with every pull fact answered.
        let tip = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut bare = local_row("feat/a", tip);
        bare.fork_only = true;
        let mut full = BranchFacts::local(
            BranchName::new("feat/b"),
            CommitId::new(tip),
            Some(CommitId::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
            false,
        );
        full.pull = Some(PullSnapshot {
            number: 7,
            url: "https://forge.example/r/pull/7".to_owned(),
            head: tip.to_owned(),
            head_matches_tip: true,
            mergeable: Some("MERGEABLE".to_owned()),
            merge_state_status: None,
            review_decision: None,
            checks: Some(CheckCounts {
                total: 3,
                pending: 1,
                conclusions: BTreeMap::from([("SUCCESS".to_owned(), 2)]),
            }),
            unresolved_review_threads: Some(2),
            template_missing: Some(vec!["Approach".to_owned()]),
        });
        full.forbidden = Some(vec![crate::forbidden::Hit {
            file: "infra/app.py".to_owned(),
            line: 9,
            term: "acme-corp".to_owned(),
            text: "deploy(\"acme-corp\")".to_owned(),
        }]);

        // Then: one line each, every column present, `-` for an unanswered fact.
        assert_eq!(
            render_branch(&bare),
            "feat/a  tip aaaaaaaaaaaa  origin absent  fork-only  no-pr  checks -  threads -  template -  forbidden -"
        );
        assert_eq!(
            render_branch(&full),
            "feat/b  tip aaaaaaaaaaaa  origin differs  pr #7 mergeable=MERGEABLE merge_state=- review=- head=matches  checks 3 (SUCCESS 2; 1 pending)  threads 2 unresolved  template missing: Approach  forbidden 1 hits: infra/app.py:9 acme-corp"
        );
        // And: a body carrying every heading renders as a fact, not a verdict.
        if let Some(pull) = full.pull.as_mut() {
            pull.template_missing = Some(Vec::new());
        }
        assert!(
            render_branch(&full).contains("  template none missing  forbidden 1 hits"),
            "was: {}",
            render_branch(&full)
        );
    }

    #[test]
    fn a_tracked_pull_of_another_author_fills_the_row() {
        // Given: `knives track --pr 41` on feat/alpha names a pull request someone
        // else opened from their fork, whose head equals our local tip; discovery
        // does not list it as ours.
        let tip = "expected0000000000000000000000000000000";
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(
                BranchName::new("their/branch"),
                open_pull(41, "someone-else", "their/branch", tip),
            )]),
            ..FakeForge::default()
        };
        let tracked = BTreeMap::from([(BranchName::new("feat/alpha"), 41)]);
        let opened = opened(&forge);
        let snapshot = batch(&opened, &tracked);
        let mut rows = [local_row("feat/alpha", tip)];

        attach_pulls(&mut rows, &snapshot, &tracked, None);

        // Then: the tracked number stands in for the primary the index lacks.
        let pull = rows[0].pull.as_ref().expect("tracked pull facts");
        assert_eq!((pull.number, pull.head_matches_tip), (41, true));
    }

    #[test]
    fn a_pull_head_that_differs_from_the_local_tip_is_a_row_fact() {
        // Given: our open pull's head is one commit behind the local bookmark.
        let head = "expected0000000000000000000000000000000";
        let forge = FakeForge {
            pull_requests: BTreeMap::from([(
                BranchName::new("feat/alpha"),
                open_pull(7, "owner", "feat/alpha", head),
            )]),
            ..FakeForge::default()
        };
        let opened = opened(&forge);
        let snapshot = batch(&opened, &NO_TRACKED);
        let mut rows = [local_row(
            "feat/alpha",
            "local000000000000000000000000000000000000",
        )];

        attach_pulls(&mut rows, &snapshot, &NO_TRACKED, None);

        // Then: the row says the head differs; it is a fact, not a finding.
        let pull = rows[0].pull.as_ref().expect("pull facts");
        assert_eq!(pull.head, head);
        assert!(!pull.head_matches_tip, "was: {pull:?}");
    }

    #[test]
    fn an_unanswered_tracked_number_leaves_the_row_without_a_pull() {
        // Given: a tracked number the forge no longer knows (deleted, or mistyped).
        let forge = FakeForge::default();
        let tracked = BTreeMap::from([(BranchName::new("feat/alpha"), 41)]);
        let opened = opened(&forge);
        let snapshot = batch(&opened, &tracked);
        let mut rows = [local_row(
            "feat/alpha",
            "local000000000000000000000000000000000000",
        )];

        attach_pulls(&mut rows, &snapshot, &tracked, None);

        assert!(rows[0].pull.is_none(), "was: {:?}", rows[0].pull);
    }

    #[test]
    fn a_template_is_held_against_the_body_case_insensitively() {
        let template = Template {
            file: ".github/pull_request_template.md".to_owned(),
            headings: vec!["Overview".to_owned(), "Approach".to_owned()],
        };

        assert_eq!(
            template.missing_from("## overview\nfix\n"),
            ["Approach".to_owned()]
        );
        assert!(template.missing_from("# Overview\n# Approach\n").is_empty());
    }

    #[test]
    fn headings_skip_html_comments_and_fenced_code() {
        let text = "\
<!-- Fill in every section.
## Not a heading
-->
# Overview <!-- required -->
<!-- ## also hidden -->
```sh
# a shell comment
```
  ```
## inside an indented fence
  ```
## Testing
";

        assert_eq!(headings(text).collect::<Vec<_>>(), ["Overview", "Testing"]);
    }
}
