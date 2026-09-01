//! The `gh`-backed forge: queries, envelopes, parsers, and the process cap.
//!
//! Everything here is specific to the hosting service's command line tool and
//! its GraphQL payload shapes — including `CheckRun`'s deserializer, which
//! decodes the `__typename`-tagged rollup contexts. The domain types the
//! parsers produce live in [`crate::forge`]; nothing outside this module
//! builds a query or decodes an envelope.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Condvar, Mutex};

use serde::{Deserialize, Deserializer};

use super::{
    CheckRun, ChecksSummary, CommitOids, ConsumerHead, DiffTotals, Forge, ForgeError, PullDetails,
    PullFacts, PullRequest, PullSummary, RepoIdentity, SweepEntry, SweepPage, TimelineEvent,
    TimelineEventKind,
};
use crate::consumer_pins::ConsumerPinSource;
const PR_STATE: &str = "all";
// headRepositoryOwner is what makes a pull request ours or someone else's. Without
// it, ownership was inferred from the head branch name, so an outside contributor
// whose branch is called `main` was tracked as our work.

// The wide lists never ask for mergeable/mergeStateStatus: GitHub computes them
// lazily per pull request, which made the 300-row list cost 16s on one fork and
// deterministically 502 on another. Merge-state is live-batch-only (I2).
const PR_SUMMARY_FIELDS: &str = "number,state,reviewDecision,headRefName,headRefOid,updatedAt,\
     isDraft,url,headRepositoryOwner,baseRefName,mergeCommit";
const SUMMARY_LIST_ARGS: [&str; 8] = [
    "pr",
    "list",
    "--state",
    PR_STATE,
    "--limit",
    "300",
    "--json",
    PR_SUMMARY_FIELDS,
];

pub const fn summary_list_args() -> &'static [&'static str; 8] {
    &SUMMARY_LIST_ARGS
}

/// The fields asked for by the cheap pull-request list.
pub const fn summary_fields() -> &'static str {
    PR_SUMMARY_FIELDS
}
#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum CheckRunPayload {
    CheckRun {
        #[serde(default)]
        name: String,
        #[serde(default)]
        conclusion: Option<String>,
    },
    StatusContext {
        #[serde(default)]
        context: String,
        #[serde(default)]
        state: String,
    },
}

enum NormalizedCheckState {
    Unfinished,
    Finished(String),
}

impl NormalizedCheckState {
    fn from_check_run(conclusion: Option<String>) -> Self {
        conclusion.map_or(Self::Unfinished, Self::Finished)
    }

    fn from_status_context(state: String) -> Self {
        if state.eq_ignore_ascii_case("PENDING") || state.eq_ignore_ascii_case("EXPECTED") {
            Self::Unfinished
        } else {
            Self::Finished(state)
        }
    }

    fn into_conclusion(self) -> Option<String> {
        match self {
            Self::Unfinished => None,
            Self::Finished(conclusion) => Some(conclusion),
        }
    }
}

impl<'de> Deserialize<'de> for CheckRun {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // An unknown typename rejects the complete rollup, including any earlier known
        // failures. Reporting unavailable checks is preferable to silently calling a
        // known-red pull request clean when the forge adds an unrecognised variant.
        let (name, state) = match CheckRunPayload::deserialize(deserializer)? {
            CheckRunPayload::CheckRun { name, conclusion } => {
                (name, NormalizedCheckState::from_check_run(conclusion))
            }
            CheckRunPayload::StatusContext { context, state } => {
                (context, NormalizedCheckState::from_status_context(state))
            }
        };
        Ok(Self {
            name,
            conclusion: state.into_conclusion(),
        })
    }
}
/// Backed by the hosting service's command line tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct CliForge;
/// Medians from three real ~30-number trials against a busy upstream fork
/// (2026-08-30):
///
/// | Chunk size | Median |
/// | ---: | ---: |
/// | 40 | 4.018s |
/// | 15 | 2.857s |
/// | 10 | 2.010s |
/// | 8 | 1.879s |
///
/// Eight was the fastest configuration.
///
/// Facts-fragment cost probe: three cold-cache runs per fork (2026-08-30).
///
/// | Fork | Installed median | Candidate median | Change |
/// | --- | ---: | ---: | ---: |
/// | Busy fork one | 8.579s | 8.945s | +4.3% |
/// | Busy fork two | 14.830s | 16.353s | +10.3% |
///
/// Outcome A: both candidate medians stayed within the 20% envelope, so the
/// status facts fragment retains the diff-stat fields.
const FACTS_BATCH_CHUNK_SIZE: usize = 8;
/// `status --all` may gather 64 repositories concurrently, and each gather
/// can start several fact-batch workers. The forge sees `gh` child processes,
/// not those gather threads: capping those children at 16 keeps concurrent
/// forge requests well below the documented 100-request budget.
const MAX_CONCURRENT_GH_PROCESSES: usize = 16;

#[derive(Debug)]
struct GhProcessSemaphore {
    available: Mutex<usize>,
    released: Condvar,
}

impl GhProcessSemaphore {
    const fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
            released: Condvar::new(),
        }
    }

    fn acquire(&'static self) -> GhProcessPermit {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available == 0 {
            available = self
                .released
                .wait(available)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *available -= 1;
        drop(available);
        GhProcessPermit { semaphore: self }
    }

    fn release(&self) {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available += 1;
        self.released.notify_one();
        drop(available);
    }
}

#[derive(Debug)]
struct GhProcessPermit {
    semaphore: &'static GhProcessSemaphore,
}

impl Drop for GhProcessPermit {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

static GH_PROCESS_SEMAPHORE: GhProcessSemaphore =
    GhProcessSemaphore::new(MAX_CONCURRENT_GH_PROCESSES);

impl CliForge {
    fn run(repo: &Path, args: &[&str]) -> Result<String, ForgeError> {
        let started = std::time::Instant::now();
        // This permit must span `output()`: it is a cap on live `gh` children,
        // so releasing it after spawning would allow running processes to exceed
        // the global limit. Callers acquire it before their scoped threads join.
        let permit = GH_PROCESS_SEMAPHORE.acquire();
        let output = Command::new("gh").args(args).current_dir(repo).output()?;
        drop(permit);
        if crate::timing::enabled() {
            eprintln!(
                "{}",
                crate::timing::call_line(started.elapsed(), repo, args)
            );
        }
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(ForgeError::Command {
            command: format!("gh {}", args.join(" ")),
            dir: repo.display().to_string(),
            code: output.status.code().unwrap_or(-1),
            // Without the stderr an authentication failure reads only as "exit
            // status 4", which cost real diagnosis time.
            stderr: if stderr.is_empty() {
                "no stderr".to_owned()
            } else {
                stderr
            },
        })
    }
}

fn joined_forge_call<T>(
    result: std::thread::Result<Result<T, ForgeError>>,
) -> Result<T, ForgeError> {
    result.map_err(|_| ForgeError::Query {
        detail: "a forge worker panicked".to_owned(),
    })?
}

impl Forge for CliForge {
    fn repo_identity(&self, repo: &Path) -> Result<RepoIdentity, ForgeError> {
        let payload = Self::run(repo, &["repo", "view", "--json", "nameWithOwner,id"])?;
        parse_identity(&payload)
    }

    fn list_pull_requests(
        &self,
        repo: &Path,
        authors: &[String],
    ) -> Result<Vec<PullSummary>, ForgeError> {
        std::thread::scope(|scope| {
            let base = scope.spawn(|| {
                let payload = Self::run(repo, &SUMMARY_LIST_ARGS)?;
                parse_summaries(&payload)
            });
            let author_lists = authors
                .iter()
                .map(|author| {
                    scope.spawn(move || {
                        let search = format!("author:{author}");
                        let args = [
                            "pr",
                            "list",
                            "--state",
                            PR_STATE,
                            "--limit",
                            "300",
                            "--search",
                            &search,
                            "--json",
                            PR_SUMMARY_FIELDS,
                        ];
                        let payload = Self::run(repo, &args)?;
                        parse_summaries(&payload)
                    })
                })
                .collect::<Vec<_>>();

            let mut pull_requests = joined_forge_call(base.join())?;
            for author_list in author_lists {
                pull_requests.extend(joined_forge_call(author_list.join())?);
            }
            dedupe_by_number(&mut pull_requests);
            Ok(pull_requests)
        })
    }

    fn sweep(&self, repo: &Path, target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
        let (owner, name) = target.split()?;
        let owner = format!("owner={owner}");
        let name = format!("name={name}");
        let query = format!("query={}", sweep_query());
        let payload = Self::run(
            repo,
            &["api", "graphql", "-f", &owner, "-f", &name, "-f", &query],
        )?;
        parse_sweep(&payload)
    }

    fn pull_facts(
        &self,
        repo: &Path,
        target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        let (owner, name) = target.split()?;
        let owner = format!("owner={owner}");
        let name = format!("name={name}");

        std::thread::scope(|scope| {
            let chunks = numbers
                .chunks(FACTS_BATCH_CHUNK_SIZE)
                .map(|chunk| {
                    let owner = &owner;
                    let name = &name;
                    scope.spawn(move || {
                        let query = format!("query={}", pull_facts_query(chunk));
                        let payload = Self::run(
                            repo,
                            &["api", "graphql", "-f", owner, "-f", name, "-f", &query],
                        )?;
                        parse_pull_facts(&payload, chunk)
                    })
                })
                .collect::<Vec<_>>();

            let mut facts = BTreeMap::new();
            for chunk in chunks {
                facts.extend(joined_forge_call(chunk.join())?);
            }
            Ok(facts)
        })
    }

    fn pull_timeline(
        &self,
        repo: &Path,
        target: &RepoIdentity,
        number: u64,
    ) -> Result<Vec<TimelineEvent>, ForgeError> {
        let (owner, name) = target.split()?;
        let owner = format!("owner={owner}");
        let name = format!("name={name}");
        let query = format!("query={}", pull_timeline_query(number));
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let payload = Self::run(
                    repo,
                    &["api", "graphql", "-f", &owner, "-f", &name, "-f", &query],
                )?;
                parse_pull_timeline(&payload, number)
            });
            joined_forge_call(worker.join())
        })
    }
}

impl ConsumerPinSource for CliForge {
    fn consumer_head(&self, repo: &Path, slug: &str) -> Result<ConsumerHead, ForgeError> {
        let Some((owner, name)) = slug.split_once('/') else {
            return Err(ForgeError::Target {
                named: slug.to_owned(),
            });
        };
        let owner = format!("owner={owner}");
        let name = format!("name={name}");
        let query = format!("query={}", consumer_head_query());
        let payload = Self::run(
            repo,
            &["api", "graphql", "-f", &owner, "-f", &name, "-f", &query],
        )?;
        parse_consumer_head(&payload)
    }

    fn file_at(
        &self,
        repo: &Path,
        slug: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<String>, ForgeError> {
        let endpoint = format!("repos/{slug}/contents/{path}?ref={commit}");
        match Self::run(
            repo,
            &["api", &endpoint, "-H", "Accept: application/vnd.github.raw"],
        ) {
            Ok(text) => Ok(Some(text)),
            Err(ForgeError::Command { stderr, .. }) if stderr.contains("HTTP 404") => Ok(None),
            Err(error) => Err(error),
        }
    }
}

/// One pull request's head-ref history. `last: 100` of only the named item
/// types is bounded by construction and does not add any fields to the wide
/// pull-request sweep.
///
/// Probed on 2026-08-30 with the final shape:
///
/// | Workload | Samples (seconds) | Median |
/// | --- | --- | ---: |
/// | force-push, delete, and restore history | 1.166, 1.160, 1.125 | 1.160s |
/// | quiet pull request | 1.071 | 1.071s |
/// | busy long-lived pull request | 1.246 | 1.246s |
///
/// Every query completed without rejection, so `last: 100` stays below the
/// two-second limit without dropping state events.
pub fn pull_timeline_query(number: u64) -> String {
    format!(
        "query($owner: String!, $name: String!) {{ \
         repository(owner: $owner, name: $name) {{ \
         pullRequest(number: {number}) {{ \
         timelineItems(last: 100, itemTypes: [HEAD_REF_FORCE_PUSHED_EVENT, \
         HEAD_REF_DELETED_EVENT, HEAD_REF_RESTORED_EVENT, CLOSED_EVENT, \
         REOPENED_EVENT, MERGED_EVENT]) {{ \
         pageInfo {{ hasPreviousPage }} nodes {{ __typename \
         ... on HeadRefForcePushedEvent {{ createdAt \
             beforeCommit {{ oid tree {{ oid }} }} afterCommit {{ oid tree {{ oid }} }} }} \
         ... on HeadRefDeletedEvent {{ createdAt }} \
         ... on HeadRefRestoredEvent {{ createdAt }} \
         ... on ClosedEvent {{ createdAt }} \
         ... on ReopenedEvent {{ createdAt }} \
         ... on MergedEvent {{ createdAt commit {{ oid }} }} }} }} }} }} }}"
    )
}

/// One page, newest-updated first.
///
/// No pagination: cursoring over a changing `UPDATED_AT` ordering can skip a
/// concurrently-updated pull request, so a page that does not span the
/// watermark abandons the delta (`snapshot::Opened::complete_with`).
pub const fn sweep_query() -> &'static str {
    "query($owner: String!, $name: String!) { \
     repository(owner: $owner, name: $name) { \
     pullRequests(orderBy: {field: UPDATED_AT, direction: DESC}, first: 100) { \
     pageInfo { hasNextPage } \
     nodes { number updatedAt state } } } }"
}

/// Full I2 fact row per aliased number.
///
/// It has every summary field plus merge state, review timeline, check rollup,
/// and the newest comment (`sync`). Alias names are not load-bearing; the
/// parser keys on the repeated `number`.
pub fn pull_facts_query(numbers: &[u64]) -> String {
    let fields: String = numbers
        .iter()
        .map(|number| format!("p{number}: pullRequest(number: {number}) {{ ...facts }}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner: String!, $name: String!) {{ \
         repository(owner: $owner, name: $name) {{ {fields} }} }} \
         fragment facts on PullRequest {{ number state reviewDecision headRefName headRefOid \
         updatedAt isDraft url headRepositoryOwner {{ login }} baseRefName mergeable \
         mergeStateStatus mergeCommit {{ oid }} additions deletions changedFiles headRef {{ name }} \
         reviews(last: 100) {{ nodes {{ submittedAt }} }} \
         commits(last: 100) {{ nodes {{ commit {{ committedDate }} }} }} \
         rollup: commits(last: 1) {{ nodes {{ commit {{ tree {{ oid }} \
         parents(first: 2) {{ pageInfo {{ hasNextPage }} nodes {{ tree {{ oid }} }} }} \
         statusCheckRollup {{ contexts(first: 100) {{ pageInfo {{ hasNextPage }} nodes {{ __typename \
         ... on CheckRun {{ name conclusion }} \
         ... on StatusContext {{ context state }} }} }} }} }} }} }} \
         comments(last: 1) {{ nodes {{ createdAt }} }} }}"
    )
}

/// Drop later duplicate summary rows, keeping the forge's freshest-first order.
fn dedupe_by_number(pull_requests: &mut Vec<PullSummary>) {
    let mut seen = std::collections::BTreeSet::new();
    pull_requests.retain(|pull_request| seen.insert(pull_request.number));
}
#[derive(Deserialize)]
struct Dated {
    #[serde(rename = "submittedAt")]
    submitted_at: Option<String>,
}

#[derive(Deserialize)]
struct Committed {
    #[serde(rename = "committedDate")]
    committed_date: String,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct Nodes<T> {
    #[serde(default, rename = "pageInfo")]
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Committed,
}

#[derive(Deserialize)]
struct RollupNode {
    commit: RollupHolder,
}

#[derive(Deserialize)]
struct Tree {
    oid: String,
}

#[derive(Deserialize)]
struct ParentNode {
    #[serde(default)]
    tree: Option<Tree>,
}

#[derive(Deserialize)]
struct RollupHolder {
    #[serde(default)]
    tree: Option<Tree>,
    #[serde(default)]
    parents: Option<Nodes<ParentNode>>,
    #[serde(default, rename = "statusCheckRollup")]
    rollup: Option<Contexts>,
}

#[derive(Deserialize, Default)]
struct PageInfo {
    #[serde(default, rename = "hasNextPage")]
    has_next_page: bool,
}

#[derive(Deserialize)]
struct Contexts {
    #[serde(default)]
    contexts: Option<Nodes<CheckRun>>,
}

#[derive(Deserialize)]
struct QueryFailure {
    #[serde(default)]
    message: String,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    path: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct IdentityPayload {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
    id: String,
}

#[derive(Deserialize)]
struct ConsumerHeadTarget {
    oid: String,
}

#[derive(Deserialize)]
struct ConsumerHeadReference {
    name: String,
    target: ConsumerHeadTarget,
}

#[derive(Deserialize)]
struct ConsumerHeadRepository {
    #[serde(rename = "defaultBranchRef")]
    default_branch_ref: Option<ConsumerHeadReference>,
}

#[derive(Deserialize)]
struct ConsumerHeadData {
    repository: Option<ConsumerHeadRepository>,
}

#[derive(Deserialize)]
struct ConsumerHeadEnvelope {
    #[serde(default)]
    data: Option<ConsumerHeadData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

#[derive(Deserialize)]
struct SweepEntryPayload {
    number: u64,
    #[serde(rename = "updatedAt")]
    updated_at: String,
    state: String,
}

#[derive(Deserialize)]
struct SweepPullRequests {
    #[serde(default, rename = "pageInfo")]
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<SweepEntryPayload>,
}

#[derive(Deserialize)]
struct SweepRepository {
    #[serde(rename = "pullRequests")]
    pull_requests: SweepPullRequests,
}

#[derive(Deserialize)]
struct SweepData {
    #[serde(default)]
    repository: Option<SweepRepository>,
}

#[derive(Deserialize)]
struct SweepEnvelope {
    #[serde(default)]
    data: Option<SweepData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

#[derive(Debug, Deserialize)]
struct HeadRefNode {
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "the object's presence is the fact; the name confirms the shape"
    )]
    name: String,
}

/// Distinguishes an omitted legacy field from an answered null head ref.
#[derive(Debug, Default, Clone, Copy)]
enum HeadRef {
    #[default]
    Missing,
    Present,
    Deleted,
}

impl<'de> Deserialize<'de> for HeadRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<HeadRefNode>::deserialize(deserializer).map(|head_ref| match head_ref {
            Some(_) => Self::Present,
            None => Self::Deleted,
        })
    }
}

#[derive(Deserialize)]
struct FactsPayload {
    #[serde(flatten)]
    pull: PullRequest,
    #[serde(default)]
    additions: Option<u64>,
    #[serde(default)]
    deletions: Option<u64>,
    #[serde(default, rename = "changedFiles")]
    changed_files: Option<u64>,
    /// Missing means an old payload; null means a deleted remote head ref.
    #[serde(default, rename = "headRef")]
    head_ref: HeadRef,
    #[serde(default)]
    reviews: Option<Nodes<Dated>>,
    #[serde(default)]
    commits: Option<Nodes<CommitNode>>,
    #[serde(default)]
    rollup: Option<Nodes<RollupNode>>,
    #[serde(default)]
    comments: Option<Nodes<Created>>,
}

#[derive(Deserialize)]
struct Created {
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Deserialize)]
struct FactsData {
    #[serde(default)]
    repository: Option<BTreeMap<String, Option<FactsPayload>>>,
}

#[derive(Deserialize)]
struct FactsEnvelope {
    #[serde(default)]
    data: Option<FactsData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

#[derive(Deserialize)]
struct TimelineEnvelope {
    #[serde(default)]
    data: Option<TimelineData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

#[derive(Deserialize)]
struct TimelineData {
    #[serde(default)]
    repository: Option<TimelineRepository>,
}

#[derive(Deserialize)]
struct TimelineRepository {
    #[serde(default, rename = "pullRequest")]
    pull_request: Option<TimelinePullRequest>,
}

#[derive(Deserialize)]
struct TimelinePullRequest {
    #[serde(rename = "timelineItems")]
    timeline_items: TimelineItems,
}

#[derive(Deserialize)]
struct TimelineItems {
    #[serde(default)]
    nodes: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct TimelineCommit {
    oid: String,
    #[serde(default)]
    tree: Option<Tree>,
}

#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum TimelineEventPayload {
    #[serde(rename = "HeadRefForcePushedEvent")]
    ForcePush {
        #[serde(rename = "createdAt")]
        at: String,
        #[serde(rename = "beforeCommit")]
        before: Option<TimelineCommit>,
        #[serde(rename = "afterCommit")]
        after: Option<TimelineCommit>,
    },
    #[serde(rename = "HeadRefDeletedEvent")]
    HeadDeleted {
        #[serde(rename = "createdAt")]
        at: String,
    },
    #[serde(rename = "HeadRefRestoredEvent")]
    HeadRestored {
        #[serde(rename = "createdAt")]
        at: String,
    },
    #[serde(rename = "ClosedEvent")]
    Closed {
        #[serde(rename = "createdAt")]
        at: String,
    },
    #[serde(rename = "ReopenedEvent")]
    Reopened {
        #[serde(rename = "createdAt")]
        at: String,
    },
    #[serde(rename = "MergedEvent")]
    Merged {
        #[serde(rename = "createdAt")]
        at: String,
        commit: Option<TimelineCommit>,
    },
}

pub fn parse_identity(payload: &str) -> Result<RepoIdentity, ForgeError> {
    let identity: IdentityPayload = serde_json::from_str(payload)?;
    Ok(RepoIdentity {
        name_with_owner: identity.name_with_owner,
        id: identity.id,
    })
}

pub const fn consumer_head_query() -> &'static str {
    "query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { \
     defaultBranchRef { name target { oid } } } }"
}

pub fn parse_consumer_head(payload: &str) -> Result<ConsumerHead, ForgeError> {
    let envelope: ConsumerHeadEnvelope = serde_json::from_str(payload)?;
    if !envelope.errors.is_empty() {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let Some(reference) = envelope
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.default_branch_ref)
    else {
        return Err(ForgeError::Query {
            detail: "the consumer reply carried neither errors nor a default branch".to_owned(),
        });
    };
    Ok(ConsumerHead {
        branch: reference.name,
        commit: reference.target.oid,
    })
}

pub fn parse_sweep(payload: &str) -> Result<SweepPage, ForgeError> {
    let envelope: SweepEnvelope = serde_json::from_str(payload)?;
    if !envelope.errors.is_empty() {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let Some(pulls) = envelope
        .data
        .and_then(|data| data.repository)
        .map(|repository| repository.pull_requests)
    else {
        return Err(ForgeError::Query {
            detail: "the reply carried neither errors nor a repository".to_owned(),
        });
    };
    Ok(SweepPage {
        entries: pulls
            .nodes
            .into_iter()
            .map(|entry| SweepEntry {
                number: entry.number,
                updated_at: entry.updated_at,
                state: entry.state,
            })
            .collect(),
        has_next_page: pulls.page_info.has_next_page,
    })
}

pub fn parse_summaries(payload: &str) -> Result<Vec<PullSummary>, ForgeError> {
    Ok(serde_json::from_str(payload)?)
}

fn is_tolerable_not_found(failure: &QueryFailure, asked: &[u64]) -> bool {
    if failure.kind != "NOT_FOUND" {
        return false;
    }
    let [repository, alias] = failure.path.as_slice() else {
        return false;
    };
    let (Some(repository), Some(alias)) = (repository.as_str(), alias.as_str()) else {
        return false;
    };
    repository == "repository" && asked.iter().any(|number| alias == format!("p{number}"))
}

/// A tip is empty only when its tree is identical to its sole parent's tree.
///
/// The query caps parents at two. A continuation means an octopus merge, which
/// reads as non-empty rather than being classified from an incomplete parent list.
fn tip_commit_empty(tip: &RollupHolder) -> Option<bool> {
    let (Some(tree), Some(parents)) = (tip.tree.as_ref(), tip.parents.as_ref()) else {
        return None;
    };
    if parents.page_info.has_next_page {
        return Some(false);
    }
    let [parent] = parents.nodes.as_slice() else {
        return Some(false);
    };
    parent.tree.as_ref().map(|parent| tree.oid == parent.oid)
}

fn details_from(payload: &FactsPayload) -> Result<PullDetails, ForgeError> {
    let newest_review = payload
        .reviews
        .iter()
        .flat_map(|list| list.nodes.iter())
        .filter_map(|review| review.submitted_at.as_deref())
        .max();
    let newest_commit = payload
        .commits
        .iter()
        .flat_map(|list| list.nodes.iter())
        .map(|node| node.commit.committed_date.as_str())
        .max();
    let review_predates_head = match (newest_review, newest_commit) {
        (Some(review), Some(commit)) => Some(review < commit),
        _ => None,
    };
    let has_more_contexts = payload
        .rollup
        .iter()
        .flat_map(|list| list.nodes.iter())
        .filter_map(|node| node.commit.rollup.as_ref())
        .filter_map(|rollup| rollup.contexts.as_ref())
        .any(|contexts| contexts.page_info.has_next_page);
    if has_more_contexts {
        return Err(ForgeError::Query {
            detail: format!(
                "pull request #{} has more than 100 check contexts; refusing a truncated rollup",
                payload.pull.number
            ),
        });
    }

    let checks = Some(ChecksSummary {
        runs: payload
            .rollup
            .iter()
            .flat_map(|list| list.nodes.iter())
            .filter_map(|node| node.commit.rollup.as_ref())
            .filter_map(|rollup| rollup.contexts.as_ref())
            .flat_map(|contexts| contexts.nodes.iter())
            .cloned()
            .collect(),
    });
    let diff = match (payload.additions, payload.deletions, payload.changed_files) {
        (Some(additions), Some(deletions), Some(changed_files)) => Some(DiffTotals {
            additions,
            deletions,
            changed_files,
        }),
        _ => None,
    };
    let head_ref_deleted = match payload.head_ref {
        HeadRef::Missing => None,
        HeadRef::Present => Some(false),
        HeadRef::Deleted => Some(true),
    };
    let tip_commit_empty = payload
        .rollup
        .iter()
        .flat_map(|list| list.nodes.iter())
        .last()
        .and_then(|tip| tip_commit_empty(&tip.commit));
    Ok(PullDetails {
        review_predates_head,
        checks,
        diff,
        head_ref_deleted,
        tip_commit_empty,
    })
}

pub fn parse_pull_facts(
    payload: &str,
    asked: &[u64],
) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
    let envelope: FactsEnvelope = serde_json::from_str(payload)?;
    if envelope
        .errors
        .iter()
        .any(|failure| !is_tolerable_not_found(failure, asked))
    {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .filter(|failure| !is_tolerable_not_found(failure, asked))
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let Some(repository) = envelope.data.and_then(|data| data.repository) else {
        return Err(ForgeError::Query {
            detail: "the reply carried neither errors nor a repository".to_owned(),
        });
    };
    let mut facts = BTreeMap::new();
    for payload in repository.into_values().flatten() {
        let details = details_from(&payload)?;
        let newest_comment = payload
            .reviews
            .iter()
            .flat_map(|list| list.nodes.iter())
            .filter_map(|review| review.submitted_at.as_deref())
            .chain(
                payload
                    .comments
                    .iter()
                    .flat_map(|list| list.nodes.iter())
                    .map(|comment| comment.created_at.as_str()),
            )
            .max()
            .map(str::to_owned);
        let number = payload.pull.number;
        let _ = facts.insert(
            number,
            PullFacts {
                pull: payload.pull,
                details,
                newest_comment,
            },
        );
    }
    Ok(facts)
}

/// Decode the bounded timeline payload for its requested pull request number.
pub fn parse_pull_timeline(payload: &str, number: u64) -> Result<Vec<TimelineEvent>, ForgeError> {
    let envelope: TimelineEnvelope = serde_json::from_str(payload)?;
    if !envelope.errors.is_empty() {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let pull = envelope
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.pull_request);
    let Some(pull) = pull else {
        return Err(ForgeError::Query {
            detail: format!("the reply carried neither errors nor pull request #{number}"),
        });
    };
    pull.timeline_items
        .nodes
        .into_iter()
        .map(timeline_event)
        .collect()
}

fn timeline_event(event: serde_json::Value) -> Result<TimelineEvent, ForgeError> {
    let Some(typename) = event.get("__typename").and_then(serde_json::Value::as_str) else {
        return Err(ForgeError::Query {
            detail: "a pull timeline event had no __typename".to_owned(),
        });
    };
    match typename {
        "HeadRefForcePushedEvent"
        | "HeadRefDeletedEvent"
        | "HeadRefRestoredEvent"
        | "ClosedEvent"
        | "ReopenedEvent"
        | "MergedEvent" => {}
        unknown => {
            return Err(ForgeError::Query {
                detail: format!("the forge returned unknown pull timeline event type `{unknown}`"),
            });
        }
    }
    let event: TimelineEventPayload = serde_json::from_value(event)?;
    Ok(match event {
        TimelineEventPayload::ForcePush { at, before, after } => TimelineEvent {
            at,
            kind: TimelineEventKind::ForcePush {
                before: commit_oids(before),
                after: commit_oids(after),
            },
        },
        TimelineEventPayload::HeadDeleted { at } => TimelineEvent {
            at,
            kind: TimelineEventKind::HeadDeleted,
        },
        TimelineEventPayload::HeadRestored { at } => TimelineEvent {
            at,
            kind: TimelineEventKind::HeadRestored,
        },
        TimelineEventPayload::Closed { at } => TimelineEvent {
            at,
            kind: TimelineEventKind::Closed,
        },
        TimelineEventPayload::Reopened { at } => TimelineEvent {
            at,
            kind: TimelineEventKind::Reopened,
        },
        TimelineEventPayload::Merged { at, commit } => TimelineEvent {
            at,
            kind: TimelineEventKind::Merged {
                commit: commit.map(|commit| commit.oid),
            },
        },
    })
}

fn commit_oids(commit: Option<TimelineCommit>) -> CommitOids {
    let Some(commit) = commit else {
        return CommitOids {
            commit: "unknown".to_owned(),
            tree: "unknown".to_owned(),
        };
    };
    CommitOids {
        commit: commit.oid,
        tree: commit
            .tree
            .map_or_else(|| "unknown".to_owned(), |tree| tree.oid),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;
    use crate::config::test_support::{EnvironmentGuard, environment_lock};
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::PermissionsExt as _;

    struct FakeGhGate {
        directory: tempfile::TempDir,
        entered: std::path::PathBuf,
        gate: std::path::PathBuf,
        max: std::path::PathBuf,
    }

    impl FakeGhGate {
        fn new(environment: &EnvironmentGuard) -> Self {
            let directory = tempfile::tempdir().expect("temporary fake gh directory");
            let gh = directory.path().join("gh");
            let lock = directory.path().join("lock");
            let current = directory.path().join("current");
            let max = directory.path().join("max");
            let entered = directory.path().join("entered");
            let gate = directory.path().join("gate");
            fs::write(&current, "0\n").expect("initialize fake gh counter");
            fs::write(&max, "0\n").expect("initialize fake gh maximum");
            for fifo in [&entered, &gate] {
                let status = Command::new("mkfifo")
                    .arg(fifo)
                    .status()
                    .expect("create fake gh synchronization fifo");
                assert!(status.success(), "create fake gh synchronization fifo");
            }
            fs::write(
                &gh,
                r#"#!/bin/sh
set -eu
lock_counter() {
    while ! mkdir "$FAKE_GH_LOCK" 2>/dev/null; do
        :
    done
}
unlock_counter() {
    rmdir "$FAKE_GH_LOCK"
}
lock_counter
active=$(cat "$FAKE_GH_CURRENT")
active=$((active + 1))
printf '%s\n' "$active" > "$FAKE_GH_CURRENT"
seen=$(cat "$FAKE_GH_MAX")
if [ "$active" -gt "$seen" ]; then
    printf '%s\n' "$active" > "$FAKE_GH_MAX"
fi
unlock_counter
exec 3>"$FAKE_GH_ENTERED"
printf . >&3
IFS= read -r _ < "$FAKE_GH_GATE"
exec 3>&-
lock_counter
active=$(cat "$FAKE_GH_CURRENT")
printf '%s\n' "$((active - 1))" > "$FAKE_GH_CURRENT"
unlock_counter
printf '{}'
"#,
            )
            .expect("write fake gh");
            fs::set_permissions(&gh, fs::Permissions::from_mode(0o755))
                .expect("make fake gh executable");
            environment.set(
                "PATH",
                &format!(
                    "{}:{}",
                    directory.path().display(),
                    std::env::var("PATH").expect("read PATH")
                ),
            );
            environment.set("FAKE_GH_LOCK", lock.to_str().expect("utf-8 lock path"));
            environment.set(
                "FAKE_GH_CURRENT",
                current.to_str().expect("utf-8 current path"),
            );
            environment.set("FAKE_GH_MAX", max.to_str().expect("utf-8 maximum path"));
            environment.set(
                "FAKE_GH_ENTERED",
                entered.to_str().expect("utf-8 entered fifo path"),
            );
            environment.set("FAKE_GH_GATE", gate.to_str().expect("utf-8 gate fifo path"));
            Self {
                directory,
                entered,
                gate,
                max,
            }
        }

        fn repository(&self) -> &Path {
            self.directory.path()
        }

        fn wait_for_permit_holders(&self) -> fs::File {
            let mut entered = fs::File::open(&self.entered).expect("open fake gh entry fifo");
            let mut cohort = [0; MAX_CONCURRENT_GH_PROCESSES];
            entered
                .read_exact(&mut cohort)
                .expect("wait for every permit holder to enter fake gh");
            entered
        }

        fn release(&self, calls: usize) -> fs::File {
            // A read-write endpoint keeps queued release tokens in the FIFO until
            // the second cohort opens its read end; a write-only endpoint can see
            // EPIPE after the first cohort exits.
            let mut release = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.gate)
                .expect("open fake gh release fifo");
            for _ in 0..calls {
                writeln!(release, "release").expect("release fake gh child");
            }
            release
        }

        fn maximum(&self) -> usize {
            fs::read_to_string(&self.max)
                .expect("read maximum fake gh concurrency")
                .trim()
                .parse()
                .expect("parse maximum fake gh concurrency")
        }
    }

    #[test]
    fn cli_forge_limits_concurrent_gh_processes_globally() {
        use std::sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        };

        const CALLS: usize = MAX_CONCURRENT_GH_PROCESSES * 2;

        let _environment = environment_lock();
        let environment = EnvironmentGuard::capture(&[
            "PATH",
            "FAKE_GH_LOCK",
            "FAKE_GH_CURRENT",
            "FAKE_GH_MAX",
            "FAKE_GH_ENTERED",
            "FAKE_GH_GATE",
        ]);
        let fake = FakeGhGate::new(&environment);
        let start = Arc::new(Barrier::new(CALLS + 1));
        let attempts = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            let workers = (0..CALLS)
                .map(|_| {
                    let start = Arc::clone(&start);
                    let attempts = Arc::clone(&attempts);
                    let repo = fake.repository();
                    scope.spawn(move || {
                        start.wait();
                        attempts.fetch_add(1, Ordering::SeqCst);
                        CliForge::run(repo, &["api", "graphql"]).expect("fake gh succeeds");
                    })
                })
                .collect::<Vec<_>>();
            start.wait();
            while attempts.load(Ordering::SeqCst) != CALLS {
                std::thread::yield_now();
            }

            let _entered = fake.wait_for_permit_holders();
            assert_eq!(
                fake.maximum(),
                MAX_CONCURRENT_GH_PROCESSES,
                "all calls have started, but only the permitted cohort may enter fake gh"
            );
            let _release = fake.release(CALLS);
            for worker in workers {
                worker.join().expect("gh worker does not panic");
            }
        });

        let observed = fake.maximum();
        assert!(
            observed <= MAX_CONCURRENT_GH_PROCESSES,
            "observed {observed} concurrent gh processes; cap is {MAX_CONCURRENT_GH_PROCESSES}"
        );
    }
    #[test]
    fn a_summary_row_has_no_merge_state_to_read() {
        // The field split is structural: this test documents it by parsing a payload
        // that CARRIES mergeable and asserting the summary type ignores it.
        let parsed = parse_summaries(
            r#"[{"number":7,"state":"OPEN","headRefName":"feat/a",
        "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z","mergeable":"CONFLICTING"}]"#,
        )
        .expect("summary row parses");
        assert_eq!(parsed[0].number, 7); // PullSummary has no mergeable field — enforced by the compiler.
    }

    #[test]
    fn a_sweep_page_reports_order_and_continuation() {
        let page = parse_sweep(
            r#"{"data":{"repository":{"pullRequests":{"pageInfo":{"hasNextPage":true},
        "nodes":[{"number":9,"updatedAt":"2026-08-02T00:00:00Z","state":"OPEN"},
                 {"number":7,"updatedAt":"2026-08-01T00:00:00Z","state":"MERGED"}]}}}}"#,
        )
        .expect("sweep page parses");
        assert!(page.has_next_page);
        assert_eq!(page.entries[0].number, 9, "newest-updated first");
        assert_eq!(page.entries[1].state, "MERGED");
    }

    /// The live batch reply's shape, with one fact payload per aliased field.
    fn facts_payload(entries: &str) -> String {
        format!("{{\"data\":{{\"repository\":{{{entries}}}}}}}")
    }
    #[test]
    fn a_null_review_decision_in_the_facts_batch_decodes_as_no_review() {
        // The forge returns explicit null for a pull request nobody reviewed; serde's
        // #[serde(default)] does not cover explicit null, and this exact reply shape
        // downgraded a whole status run to pull-state-unavailable.
        let payload = r#"{"data":{"repository":{"p134":{
        "number":134,"state":"OPEN","reviewDecision":null,
        "headRefName":"feat/x","headRefOid":"0123456789abcdef0123456789abcdef01234567",
        "updatedAt":"2026-08-30T00:00:00Z","isDraft":false,
        "url":"https://forge.example/r/pull/134",
        "headRepositoryOwner":{"login":"someone"},"baseRefName":"main",
        "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","mergeCommit":null,
        "additions":1,"deletions":0,"changedFiles":1,"headRef":{"name":"feat/x"},
        "reviews":{"nodes":[]},
        "commits":{"nodes":[{"commit":{"committedDate":"2026-08-29T00:00:00Z"}}]}
    }}}}"#;
        let facts = parse_pull_facts(payload, &[134]).expect("null reviewDecision must decode");
        assert_eq!(facts[&134].pull.review_decision, "");
    }

    #[test]
    fn null_merge_facts_remain_explicitly_unknown() {
        // A null merge fact is an incomplete forge answer, not an empty string that
        // downstream status logic could mistake for non-conflicting or on-base.
        let payload = facts_payload(
            r#""p134":{"number":134,"state":"OPEN","headRefName":"feat/x",
        "headRefOid":"0123456789abcdef0123456789abcdef01234567",
        "updatedAt":"2026-08-30T00:00:00Z","baseRefName":null,
        "mergeable":null,"mergeStateStatus":null}"#,
        );

        let facts = parse_pull_facts(&payload, &[134]).expect("null merge facts decode");
        let encoded = serde_json::to_value(&facts[&134].pull).expect("pull serialises");

        assert!(encoded["mergeable"].is_null(), "was: {encoded}");
        assert!(encoded["mergeStateStatus"].is_null(), "was: {encoded}");
        assert!(encoded["baseRefName"].is_null(), "was: {encoded}");
    }

    #[test]
    fn a_null_required_string_in_the_facts_batch_fails_to_decode() {
        let payload = facts_payload(
            r#""p134":{"number":134,"state":null,"headRefName":"feat/x",
        "headRefOid":"0123456789abcdef0123456789abcdef01234567",
        "updatedAt":"2026-08-30T00:00:00Z"}"#,
        );

        let error = parse_pull_facts(&payload, &[134])
            .expect_err("a null required field must make the batch unavailable");

        assert!(matches!(error, ForgeError::Parse { .. }), "was: {error}");
        assert!(
            error
                .to_string()
                .contains("invalid type: null, expected a string"),
            "was: {error}"
        );
    }

    #[test]
    fn facts_carry_the_full_row_the_details_and_the_newest_comment() {
        // One alias carrying every summary field plus mergeable CONFLICTING,
        // headRepositoryOwner{login}, a review newer than the newest comment.
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","reviewDecision":"APPROVED","headRefName":"feat/a",
        "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z","isDraft":false,"url":"u",
        "headRepositoryOwner":{"login":"our-org"},"baseRefName":"main","mergeable":"CONFLICTING",
        "mergeStateStatus":"DIRTY","mergeCommit":null,
        "reviews":{"nodes":[{"submittedAt":"2026-08-02T00:00:00Z"}]},
        "commits":{"nodes":[{"commit":{"committedDate":"2026-08-01T12:00:00Z"}}]},
        "rollup":{"nodes":[]},"comments":{"nodes":[{"createdAt":"2026-07-30T00:00:00Z"}]}}"#,
        );
        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");
        let fact = &facts[&7];
        assert_eq!(fact.pull.mergeable.as_deref(), Some("CONFLICTING"));
        assert_eq!(
            fact.pull
                .head_repository_owner
                .as_ref()
                .map(|owner| owner.login.as_str()),
            Some("our-org")
        );
        assert_eq!(
            fact.details.review_predates_head,
            Some(false),
            "review is newer than the head"
        );
        assert_eq!(
            fact.newest_comment.as_deref(),
            Some("2026-08-02T00:00:00Z"),
            "the review outranks the older comment"
        );
    }

    #[test]
    fn facts_carry_diff_totals_head_ref_presence_and_tip_emptiness() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
        "updatedAt":"2026-08-01T00:00:00Z",
        "additions":0,"deletions":0,"changedFiles":0,"headRef":null,
        "rollup":{"nodes":[{"commit":{"additions":0,"deletions":0,"tree":{"oid":"same"},
        "parents":{"nodes":[{"tree":{"oid":"same"}}]}}}]}}"#,
        );
        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");
        let details = &facts[&7].details;
        assert_eq!(
            details.diff,
            Some(crate::forge::DiffTotals {
                additions: 0,
                deletions: 0,
                changed_files: 0
            }),
            "an answered zero diff is a fact, not an absence"
        );
        assert_eq!(
            details.head_ref_deleted,
            Some(true),
            "headRef null means the ref is gone"
        );
        assert_eq!(details.tip_commit_empty, Some(true));
    }

    #[test]
    fn facts_do_not_mistake_zero_line_rename_for_an_empty_tip() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
        "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
        "additions":0,"deletions":0,"tree":{"oid":"renamed"},
        "parents":{"nodes":[{"tree":{"oid":"original"}}]}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");

        assert_eq!(facts[&7].details.tip_commit_empty, Some(false));
    }

    #[test]
    fn facts_do_not_mistake_a_merge_tip_for_an_empty_tip() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
        "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
        "additions":0,"deletions":0,"tree":{"oid":"merged"},
        "parents":{"nodes":[{"tree":{"oid":"left"}},{"tree":{"oid":"right"}}]}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");

        assert_eq!(facts[&7].details.tip_commit_empty, Some(false));
    }

    #[test]
    fn facts_do_not_mistake_a_root_tip_for_an_empty_tip() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
        "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
        "additions":0,"deletions":0,"tree":{"oid":"root"},"parents":{"nodes":[]}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");

        assert_eq!(facts[&7].details.tip_commit_empty, Some(false));
    }

    #[test]
    fn facts_do_not_guess_about_an_octopus_tip() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
        "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
        "additions":0,"deletions":0,"tree":{"oid":"octopus"},
        "parents":{"pageInfo":{"hasNextPage":true},
        "nodes":[{"tree":{"oid":"one"}},{"tree":{"oid":"two"}}]}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");

        assert_eq!(facts[&7].details.tip_commit_empty, Some(false));
    }

    #[test]
    fn facts_missing_the_new_fields_answer_none_not_zero() {
        // An old recorded payload (or a forge that refused the fields) must read as
        // "not answered", never as "empty diff" — the not-consulted/nothing-found
        // distinction the whole forge module is built on.
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
        "updatedAt":"2026-08-01T00:00:00Z"}"#,
        );
        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");
        let details = &facts[&7].details;
        assert_eq!(details.diff, None);
        assert_eq!(details.head_ref_deleted, None);
        assert_eq!(details.tip_commit_empty, None);
    }

    #[test]
    fn a_failing_check_is_distinct_from_an_empty_rollup_in_facts() {
        let payload = facts_payload(
            r#""p11":{"number":11,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":{"contexts":{"nodes":[
            {"__typename":"CheckRun","conclusion":"FAILURE","name":"build"},
            {"__typename":"CheckRun","conclusion":"SUCCESS","name":"lint"}]}}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[11]).expect("facts parse");

        let checks = facts[&11].details.checks.as_ref().expect("consulted");
        assert!(checks.failing(), "a FAILURE conclusion is failing");
        assert_eq!(checks.failed_names(), vec!["build".to_owned()]);
        assert!(checks.ran());
    }

    #[test]
    fn a_null_check_run_conclusion_is_pending_in_facts() {
        let payload = facts_payload(
            r#""p4908":{"number":4908,"state":"OPEN","headRefName":"feat/running","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":{"contexts":{"nodes":[{"__typename":"CheckRun",
            "name":"live-build","conclusion":null}]}}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[4908]).expect("facts parse");

        let checks = facts[&4908].details.checks.as_ref().expect("consulted");
        assert!(checks.ran(), "the returned check has started");
        assert!(checks.pending(), "a null conclusion is still running");
        assert!(!checks.failing(), "a running check is not failing");
    }

    #[test]
    fn pending_legacy_status_contexts_serialize_as_unfinished_check_runs() {
        let payload = facts_payload(
            r#""p4909":{"number":4909,"state":"OPEN","headRefName":"feat/running","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":{"contexts":{"nodes":[
            {"__typename":"StatusContext","context":"legacy-pending","state":"PENDING"},
            {"__typename":"StatusContext","context":"legacy-expected","state":"EXPECTED"}
            ]}}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[4909]).expect("facts parse");

        let checks = facts[&4909].details.checks.as_ref().expect("consulted");
        assert!(checks.pending(), "the status contexts are unfinished");
        assert!(!checks.failing(), "the status contexts are not failing");
        assert_eq!(
            serde_json::to_value(checks).expect("checks serialize"),
            serde_json::json!([
                {"name":"legacy-pending","conclusion":null},
                {"name":"legacy-expected","conclusion":null}
            ])
        );
    }

    #[test]
    fn an_error_status_context_is_failing_in_facts() {
        let payload = facts_payload(
            r#""p11":{"number":11,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":{"contexts":{"nodes":[{"__typename":"StatusContext",
            "context":"legacy-ci","state":"ERROR"}]}}}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[11]).expect("facts parse");

        let checks = facts[&11].details.checks.as_ref().expect("consulted");
        assert!(checks.failing(), "an ERROR state is failing");
        assert_eq!(checks.failed_names(), vec!["legacy-ci".to_owned()]);
    }

    #[test]
    fn an_empty_rollup_is_consulted_while_an_absent_facts_alias_is_not() {
        let payload = facts_payload(
            r#""p12":{"number":12,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":null}}]}},"p13":null"#,
        );

        let facts = parse_pull_facts(&payload, &[12, 13]).expect("facts parse");

        let checks = facts[&12].details.checks.as_ref().expect("consulted");
        assert!(!checks.failing(), "an absent rollup is not a failure");
        assert!(!checks.ran(), "an absent rollup means nothing ran");
        assert!(
            !facts.contains_key(&13),
            "a number the reply did not carry is not consulted, not empty"
        );
    }

    #[test]
    fn a_facts_reply_with_neither_errors_nor_a_repository_answers_nothing_loudly() {
        let error = parse_pull_facts(r#"{"data":{}}"#, &[7])
            .expect_err("a reply about nothing is not an answer");

        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");
        assert!(
            error
                .to_string()
                .contains("neither errors nor a repository"),
            "was: {error}"
        );
    }

    #[test]
    fn a_facts_rollup_with_more_than_one_page_of_contexts_is_unavailable() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","rollup":{"nodes":[{"commit":{
            "statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":true},"nodes":[
            {"__typename":"CheckRun","name":"green","conclusion":"SUCCESS"}]}}}}]}}"#,
        );

        let error = parse_pull_facts(&payload, &[7])
            .expect_err("a paginated check rollup must not render as complete");

        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");
        assert!(error.to_string().contains("#7"), "was: {error}");
        assert!(error.to_string().contains("more than 100"), "was: {error}");
    }

    #[test]
    fn facts_keep_stale_current_and_incomparable_reviews_distinct_in_any_node_order() {
        let payload = facts_payload(
            r#""p1":{"number":1,"state":"OPEN","headRefName":"feat/stale","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z","reviews":{"nodes":[
            {"submittedAt":"2026-07-01T00:00:00Z"}]},"commits":{"nodes":[{"commit":{
            "committedDate":"2026-07-02T00:00:00Z"}}]}},
            "p2":{"number":2,"state":"OPEN","headRefName":"feat/current","headRefOid":"bb",
            "updatedAt":"2026-08-01T00:00:00Z","reviews":{"nodes":[
            {"submittedAt":"2026-07-03T00:00:00Z"}]},"commits":{"nodes":[{"commit":{
            "committedDate":"2026-07-02T00:00:00Z"}}]}},
            "p3":{"number":3,"state":"OPEN","headRefName":"feat/unreviewed","headRefOid":"cc",
            "updatedAt":"2026-08-01T00:00:00Z","reviews":{"nodes":[]},"commits":{"nodes":[{"commit":{
            "committedDate":"2026-07-02T00:00:00Z"}}]}},
            "p4":{"number":4,"state":"OPEN","headRefName":"feat/unordered","headRefOid":"dd",
            "updatedAt":"2026-08-01T00:00:00Z","reviews":{"nodes":[
            {"submittedAt":"2026-07-01T00:00:00Z"},{"submittedAt":"2026-07-05T00:00:00Z"}]},
            "commits":{"nodes":[{"commit":{"committedDate":"2026-07-02T00:00:00Z"}},
            {"commit":{"committedDate":"2026-07-04T00:00:00Z"}}]}}"#,
        );

        let facts = parse_pull_facts(&payload, &[1, 2, 3, 4]).expect("facts parse");

        assert_eq!(facts[&1].details.review_predates_head, Some(true));
        assert_eq!(facts[&2].details.review_predates_head, Some(false));
        assert_eq!(facts[&3].details.review_predates_head, None);
        assert_eq!(
            facts[&4].details.review_predates_head,
            Some(false),
            "the newest review and commit decide it, not the first listed nodes"
        );
    }

    #[test]
    fn a_not_found_alias_is_absent_and_does_not_fail_the_batch() {
        let payload =
            r#"{"data":{"repository":{"p7":{"number":7,"state":"OPEN","headRefName":"feat/a",
            "headRefOid":"aa","updatedAt":"2026-08-01T00:00:00Z"},"p300":null}},
            "errors":[{"type":"NOT_FOUND","path":["repository","p300"],"message":"not found"}]}"#
                .to_owned();
        let facts =
            parse_pull_facts(&payload, &[7, 300]).expect("a missing asked alias is tolerated");
        assert!(facts.contains_key(&7));
        assert!(!facts.contains_key(&300));
    }

    #[test]
    fn any_other_error_fails_the_whole_batch() {
        let payload = r#"{"data":{"repository":{"p7":null}},"errors":[
            {"type":"RATE_LIMITED","path":["repository","p7"],"message":"rate limited"}]}"#;
        let error = parse_pull_facts(payload, &[7])
            .expect_err("only asked NOT_FOUND aliases are tolerated");
        assert!(matches!(error, ForgeError::Query { .. }), "was: {error}");
        assert!(error.to_string().contains("rate limited"), "was: {error}");
    }

    #[test]
    fn the_newest_comment_is_the_max_of_comments_and_reviews() {
        let payload = facts_payload(
            r#""p7":{"number":7,"state":"OPEN","headRefName":"feat/a","headRefOid":"aa",
            "updatedAt":"2026-08-01T00:00:00Z",
            "reviews":{"nodes":[{"submittedAt":"2026-08-02T00:00:00Z"}]},
            "comments":{"nodes":[{"createdAt":"2026-08-03T00:00:00Z"}]}}"#,
        );
        let facts = parse_pull_facts(&payload, &[7]).expect("facts parse");
        assert_eq!(
            facts[&7].newest_comment.as_deref(),
            Some("2026-08-03T00:00:00Z")
        );
    }

    #[test]
    fn consumer_head_query_parses_the_default_branch_and_commit() {
        let head = parse_consumer_head(
            r#"{"data":{"repository":{"defaultBranchRef":{"name":"main","target":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}}}}"#,
        )
        .expect("consumer head parses");

        assert_eq!(head.branch, "main");
        assert_eq!(head.commit, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let query = consumer_head_query();
        assert!(query.contains("defaultBranchRef"), "was: {query}");
        assert!(query.contains("target { oid }"), "was: {query}");
    }

    #[test]
    fn a_repo_identity_exposes_and_splits_the_forges_own_name() {
        let identity = parse_identity(r#"{"nameWithOwner":"our-org/some-repo","id":"R_kgDOxyz"}"#)
            .expect("identity parses");
        assert_eq!(
            identity.split().expect("owner/repo"),
            ("our-org", "some-repo")
        );

        let error = RepoIdentity {
            name_with_owner: "bare".to_owned(),
            id: "R_kgDOxyz".to_owned(),
        }
        .split()
        .expect_err("an unsplittable identity cannot be queried");
        assert!(matches!(error, ForgeError::Target { .. }), "was: {error}");
    }
    #[test]
    fn cache_list_contract_omits_live_merge_state_fields() {
        let args = summary_list_args();
        assert_eq!(
            args,
            &[
                "pr",
                "list",
                "--state",
                "all",
                "--limit",
                "300",
                "--json",
                "number,state,reviewDecision,headRefName,headRefOid,updatedAt,isDraft,url,\
     headRepositoryOwner,baseRefName,mergeCommit"
            ]
        );
        assert!(!summary_fields().contains("mergeable"));
        assert!(!summary_fields().contains("mergeStateStatus"));
    }
    #[test]
    fn facts_batches_30_numbers_into_four_bounded_queries() {
        let numbers: Vec<u64> = (1..=30).collect();
        let sizes: Vec<usize> = numbers
            .chunks(FACTS_BATCH_CHUNK_SIZE)
            .map(<[u64]>::len)
            .collect();

        assert_eq!(sizes, [8, 8, 8, 6]);
    }

    #[test]
    fn live_queries_request_the_required_fact_and_sweep_shapes() {
        let sweep = sweep_query();
        assert!(
            sweep.contains("orderBy: {field: UPDATED_AT, direction: DESC}"),
            "was: {sweep}"
        );
        assert!(sweep.contains("first: 100"), "was: {sweep}");
        let facts = pull_facts_query(&[7, 300]);
        assert!(facts.contains("p7: pullRequest(number: 7)"), "was: {facts}");
        assert!(
            facts.contains("p300: pullRequest(number: 300)"),
            "was: {facts}"
        );
        assert!(facts.contains("mergeable"), "was: {facts}");
        assert!(facts.contains("mergeStateStatus"), "was: {facts}");
        assert!(facts.contains("comments(last: 1)"), "was: {facts}");
    }

    fn timeline_payload(nodes: &str) -> String {
        format!(
            "{{\"data\":{{\"repository\":{{\"pullRequest\":{{\"timelineItems\":\
             {{\"pageInfo\":{{\"hasPreviousPage\":false}},\"nodes\":[{nodes}]}}}}}}}}}}"
        )
    }

    #[test]
    fn a_force_push_carries_before_and_after_commit_and_tree_oids() {
        let payload = timeline_payload(
            r#"{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-08-30T22:41:02Z",
            "beforeCommit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "tree":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},
            "afterCommit":{"oid":"cccccccccccccccccccccccccccccccccccccccc",
            "tree":{"oid":"dddddddddddddddddddddddddddddddddddddddd"}}}"#,
        );

        let events = parse_pull_timeline(&payload, 7).expect("timeline parses");

        assert_eq!(
            events,
            vec![TimelineEvent {
                at: "2026-08-30T22:41:02Z".to_owned(),
                kind: TimelineEventKind::ForcePush {
                    before: CommitOids {
                        commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                        tree: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                    },
                    after: CommitOids {
                        commit: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
                        tree: "dddddddddddddddddddddddddddddddddddddddd".to_owned(),
                    },
                },
            }]
        );
    }

    #[test]
    fn delete_and_restore_events_keep_the_forges_chronological_order() {
        let payload = timeline_payload(
            r#"{"__typename":"HeadRefDeletedEvent","createdAt":"2026-08-30T22:43:13Z"},
            {"__typename":"HeadRefRestoredEvent","createdAt":"2026-08-30T22:44:11Z"}"#,
        );

        let events = parse_pull_timeline(&payload, 7).expect("timeline parses");

        assert_eq!(
            events,
            vec![
                TimelineEvent {
                    at: "2026-08-30T22:43:13Z".to_owned(),
                    kind: TimelineEventKind::HeadDeleted,
                },
                TimelineEvent {
                    at: "2026-08-30T22:44:11Z".to_owned(),
                    kind: TimelineEventKind::HeadRestored,
                },
            ]
        );
    }

    #[test]
    fn an_unknown_timeline_event_type_rejects_the_whole_history() {
        let payload = timeline_payload(
            r#"{"__typename":"NewForgeTimelineEvent","createdAt":"2026-08-30T22:43:13Z"}"#,
        );

        let error = parse_pull_timeline(&payload, 7).expect_err("unknown event must not disappear");

        assert!(matches!(error, ForgeError::Query { .. }), "was: {error}");
        assert!(
            error.to_string().contains("NewForgeTimelineEvent"),
            "was: {error}"
        );
    }

    #[test]
    fn a_force_push_with_a_garbage_collected_before_commit_keeps_the_event() {
        let payload = timeline_payload(
            r#"{"__typename":"HeadRefForcePushedEvent","createdAt":"2026-08-30T22:41:02Z",
            "beforeCommit":null,"afterCommit":{"oid":"cccccccccccccccccccccccccccccccccccccccc",
            "tree":{"oid":"dddddddddddddddddddddddddddddddddddddddd"}}}"#,
        );

        let events = parse_pull_timeline(&payload, 7).expect("missing before commit is historical");

        assert_eq!(
            events,
            vec![TimelineEvent {
                at: "2026-08-30T22:41:02Z".to_owned(),
                kind: TimelineEventKind::ForcePush {
                    before: CommitOids {
                        commit: "unknown".to_owned(),
                        tree: "unknown".to_owned(),
                    },
                    after: CommitOids {
                        commit: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
                        tree: "dddddddddddddddddddddddddddddddddddddddd".to_owned(),
                    },
                },
            }]
        );
    }

    #[test]
    fn an_absent_pull_timeline_names_the_requested_number() {
        let error = parse_pull_timeline(r#"{"data":{"repository":{"pullRequest":null}}}"#, 88)
            .expect_err("a null pull request is unavailable");

        assert!(matches!(error, ForgeError::Query { .. }), "was: {error}");
        assert!(error.to_string().contains("#88"), "was: {error}");
    }
}
