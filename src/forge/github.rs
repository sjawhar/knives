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
    CheckRun, ChecksSummary, Forge, ForgeError, PullDetails, PullFacts, PullRequest, PullSummary,
    RepoIdentity, SweepEntry, SweepPage,
};
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
         mergeStateStatus mergeCommit {{ oid }} \
         reviews(last: 100) {{ nodes {{ submittedAt }} }} \
         commits(last: 100) {{ nodes {{ commit {{ committedDate }} }} }} \
         rollup: commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ \
         contexts(first: 100) {{ pageInfo {{ hasNextPage }} nodes {{ __typename \
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
struct RollupHolder {
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

#[derive(Deserialize)]
struct FactsPayload {
    #[serde(flatten)]
    pull: PullRequest,
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

pub fn parse_identity(payload: &str) -> Result<RepoIdentity, ForgeError> {
    let identity: IdentityPayload = serde_json::from_str(payload)?;
    Ok(RepoIdentity {
        name_with_owner: identity.name_with_owner,
        id: identity.id,
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
                details: PullDetails {
                    review_predates_head,
                    checks,
                },
                newest_comment,
            },
        );
    }
    Ok(facts)
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
        assert_eq!(fact.pull.mergeable, "CONFLICTING");
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
}
